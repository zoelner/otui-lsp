//! `cargo xtask` — development tasks for otui-lsp.
//!
//! ## `gen-catalog`
//!
//! Extracts the OTML **property-name catalog** and the **named-color table** from the OTClient
//! engine C++ source and writes them as a committed, generated Rust data file
//! (`crates/otui-core/src/catalog.rs`) consumed by `otui-core::schema`.
//!
//! The generated file is committed to the repo (exactly like `tree-sitter-otui/src/parser.c`), so
//! the normal `cargo build`/`./ci.sh` never needs the engine source. This task is re-run **by
//! hand** only when a fork updates the engine — there is deliberately **no CI drift-guard** for it
//! (unlike the tree-sitter parser, whose regeneration CI enforces), because CI has no engine
//! source. Re-running against the same source is idempotent: the output is sorted and de-duped, so
//! it never churns.
//!
//! ### Leak safety
//!
//! The engine-source root is **never** hard-coded: it comes from `--src <path>` or the
//! `OTUI_ENGINE_SRC` environment variable. If neither is provided, the task prints a usage error
//! and exits non-zero. No absolute path or fork identity is baked into this tool or its output; the
//! generated provenance banner is generic.
//!
//! ### Per-fork seam (future work)
//!
//! For now this emits a **single** catalog. A future per-fork variant (see the `otui.toml` profile
//! idea in the plan) can reuse [`Catalog`] by writing multiple named tables; the extraction and the
//! `--src` plumbing already isolate "which engine tree" from "what we emit".

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regex::Regex;

/// Relative paths (under the engine-source root) whose style-dispatch chains hold the property
/// tags. `parseBaseStyle` lives in `uiwidgetbasestyle.cpp`; the `image-*` and `text-*` families are
/// dispatched by `parseImageStyle` / `parseTextStyle`, which some engine trees split into their own
/// translation units. The image/text files are treated as **optional** so the extractor still works
/// against forks that inline them into the base file.
const PROPERTY_FILES: &[(&str, bool)] = &[
    (BASE_STYLE_FILE, true),
    ("src/framework/ui/uiwidgetimage.cpp", false),
    ("src/framework/ui/uiwidgettext.cpp", false),
];

/// The widget style parser itself (`UIWidget::parseBaseStyle`). Required: it carries both the bulk
/// of the property tags and the `layout:` block handler that names the block's `type` key.
const BASE_STYLE_FILE: &str = "src/framework/ui/uiwidgetbasestyle.cpp";

/// Engine-source files (relative to the engine-source root) whose `applyStyle` reads the keys of a
/// `layout:` **block**. These keys are dispatched by the *layout* object, not by the widget style
/// parser, so they never appear in [`PROPERTY_FILES`] — without them every `cell-size:` /
/// `spacing:` / `flow:` under a `layout:` block looks like an unknown property.
///
/// The set is the **union** across the layout classes: the engine picks one layout per widget and
/// each class silently ignores the keys it does not read, so a key that belongs to a *different*
/// layout type is ignored rather than an error. Accepting the union therefore prefers a false
/// negative (never flagging `spacing:` under `type: grid`) over a false positive, matching the
/// diagnostics rule in `otui-core`.
///
/// All are optional: a fork may inline or drop a layout class, and the union just shrinks.
const LAYOUT_FILES: &[&str] = &[
    "src/framework/ui/uilayout.cpp",
    "src/framework/ui/uiboxlayout.cpp",
    "src/framework/ui/uiverticallayout.cpp",
    "src/framework/ui/uihorizontallayout.cpp",
    "src/framework/ui/uigridlayout.cpp",
    "src/framework/ui/uianchorlayout.cpp",
];

/// Engine-source directories (relative to the engine-source root) holding the **native widget**
/// classes. A widget subclass may override `onStyleApply` and dispatch OTML tags of its own — e.g.
/// `UITextEdit` reads `placeholder`, `max-length`, `multiline`; `UIItem` reads `item-id`, `virtual`.
///
/// These tags reach neither [`PROPERTY_FILES`] (they are not in the base style parser) nor the Lua
/// scanner (the class is C++, so there is no `extends` line), so without them a perfectly valid
/// `placeholder:` on a `TextEdit` looks unknown. Unlike the base catalog they are keyed **per
/// widget**: they are valid only on that class and its descendants, so `change-cursor-image` on a
/// `Button` stays unknown — as it is, the engine reads it nowhere there.
const NATIVE_WIDGET_DIRS: &[&str] = &["src/framework/ui", "src/client"];

/// Relative path (under the engine-source root) of the CSS named-color table + legacy color names.
const COLOR_FILE: &str = "src/framework/util/color.cpp";

/// Output path (relative to the workspace root) of the committed, generated catalog.
const OUTPUT_REL: &str = "crates/otui-core/src/catalog.rs";

/// The extracted, sorted, de-duped catalog ready to be rendered to Rust source.
struct Catalog {
    properties: Vec<String>,
    /// The OTML property tags whose value the engine parses as a color (the style-dispatch sites
    /// that call `node->value<Color>()` or `Color(node->value())`, e.g. `color`, `background`,
    /// `border-color*`, `icon-color`, `image-color`, `ttf-stroke-color`). Sorted. Used to gate
    /// named-color swatches to genuine color-value positions.
    color_properties: Vec<String>,
    /// The keys the engine reads inside a `layout:` **block** — the union of every layout class's
    /// `applyStyle` tags (`spacing`, `fit-children`, `cell-size`, `num-columns`, `flow`, …) plus the
    /// `type` key the widget style parser itself reads via `valueAt<std::string>("type", "")`.
    /// Sorted. Dispatched by the layout object, not the widget style parser, so these are disjoint
    /// from [`Catalog::properties`] and must be checked in the `layout:` block context.
    layout_properties: Vec<String>,
    /// The OTML tags each **native widget subclass** dispatches in its own `onStyleApply` override,
    /// as `(UI class name, sorted tags)`, sorted by class. Keyed per widget — unlike
    /// [`Catalog::properties`] these are valid only on that class and its descendants.
    native_widget_properties: Vec<(String, Vec<String>)>,
    /// The CSS named-color table: `(lowercased name, packed 0xRRGGBB)`, sorted by name. The value is
    /// captured from the `rgb_to_abgr(0xRRGGBB)` literal in the engine's `kCss` table.
    named_colors: Vec<(String, u32)>,
    /// The legacy engine color statics (`const Color Color::NAME = 0xAABBGGRR;`, e.g. `red`, `teal`,
    /// `darkPink`) as `(lowercased name, packed 0xRRGGBBAA)`, converted from the source's AABBGGRR
    /// literal so alpha is preserved. Sorted by name.
    legacy_colors: Vec<(String, u32)>,
    /// Legacy color names recognized by the engine but with no extractable RGB value (the
    /// `transparent` alias, matched as `key == "transparent"` in `css_lookup`) — kept names-only for
    /// `is_named_color` membership. Lowercased, sorted.
    legacy_color_names: Vec<String>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("gen-catalog") => match gen_catalog(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(msg) => {
                eprintln!("xtask gen-catalog: {msg}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("xtask: unknown task '{other}'. Available: gen-catalog");
            ExitCode::FAILURE
        }
        None => {
            eprintln!(
                "usage: cargo xtask <task>\n  tasks:\n    gen-catalog --src <engine-source-root>   \
                 generate the OTUI property/color catalog"
            );
            ExitCode::FAILURE
        }
    }
}

fn gen_catalog(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    // --- resolve the engine-source root (never hard-coded; leak-safety requirement) -------------
    let mut src: Option<String> = std::env::var("OTUI_ENGINE_SRC")
        .ok()
        .filter(|s| !s.is_empty());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--src" => {
                src = Some(args.next().ok_or("`--src` requires a path argument")?);
            }
            other if other.starts_with("--src=") => {
                src = Some(other["--src=".len()..].to_string());
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    let src = src.ok_or(
        "no engine source given. Pass `--src <path>` or set OTUI_ENGINE_SRC to the OTClient \
         engine-source root.",
    )?;
    let src = PathBuf::from(src);
    if !src.is_dir() {
        return Err(format!(
            "engine source root '{}' is not a directory",
            src.display()
        ));
    }

    // --- extract ---------------------------------------------------------------------------------
    let catalog = extract(&src)?;

    // --- render + write --------------------------------------------------------------------------
    let workspace_root = workspace_root();
    let out_path = workspace_root.join(OUTPUT_REL);
    let rendered = render(&catalog);
    std::fs::write(&out_path, rendered)
        .map_err(|e| format!("failed to write '{}': {e}", out_path.display()))?;

    println!(
        "gen-catalog: wrote {} properties ({} color-typed), {} layout-block keys, {} native \
         widgets, {} named colors, {} legacy colors and {} legacy names to {}",
        catalog.properties.len(),
        catalog.color_properties.len(),
        catalog.layout_properties.len(),
        catalog.native_widget_properties.len(),
        catalog.named_colors.len(),
        catalog.legacy_colors.len(),
        catalog.legacy_color_names.len(),
        OUTPUT_REL
    );
    Ok(())
}

/// Read the engine files under `src` and pull out the property tags and color names.
fn extract(src: &Path) -> Result<Catalog, String> {
    // Property tags: every string compared against the OTML node tag in a style-dispatch chain,
    // i.e. `node->tag() == "..."` or a local `tag == "..."`. Prefix dispatches (`starts_with(...)`
    // for `anchors.`, `@`, `&`) are intentionally NOT captured — those are handled by their own
    // closed sets in `schema`, not the property catalog.
    let prop_re =
        Regex::new(r#"\btag\s*(?:\(\))?\s*==\s*"([^"]+)""#).expect("valid property regex");

    let mut properties: Vec<String> = Vec::new();
    let mut color_properties: Vec<String> = Vec::new();
    for (rel, required) in PROPERTY_FILES {
        let path = src.join(rel);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                if *required {
                    return Err(format!("failed to read '{}': {e}", path.display()));
                }
                eprintln!(
                    "gen-catalog: note: optional file '{rel}' not found ({e}); skipping its \
                     property tags."
                );
                continue;
            }
        };
        let stripped = strip_comments(&text);
        for caps in prop_re.captures_iter(&stripped) {
            properties.push(caps[1].to_string());
        }
        color_properties.extend(extract_color_properties(&stripped));
    }
    if properties.is_empty() {
        return Err(
            "extracted zero property tags — is the engine source layout as expected?".into(),
        );
    }
    if color_properties.is_empty() {
        return Err(
            "extracted zero color-typed properties — is the style-dispatch layout as expected?"
                .into(),
        );
    }

    // Layout-block keys: the same `node->tag() == "..."` dispatch, but in the layout classes'
    // `applyStyle` rather than the widget style parser, plus the `type` key the widget style parser
    // reads out of the block itself.
    let mut layout_properties: Vec<String> = Vec::new();
    for rel in LAYOUT_FILES {
        let path = src.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!(
                "gen-catalog: note: optional layout file '{rel}' not found; skipping its keys."
            );
            continue;
        };
        let stripped = strip_comments(&text);
        for caps in prop_re.captures_iter(&stripped) {
            layout_properties.push(caps[1].to_string());
        }
    }
    layout_properties.extend(extract_layout_type_keys(src)?);
    if layout_properties.is_empty() {
        return Err(
            "extracted zero layout-block keys — is the layout `applyStyle` layout as expected?"
                .into(),
        );
    }

    // Colors: the CSS table entries `{"name", rgb_to_abgr(0xRRGGBB)}` (name + RGB value), the legacy
    // engine color statics `const Color Color::NAME = 0xAABBGGRR` (name + RGBA value, alpha
    // preserved), and any remaining recognized names (the `transparent` alias). Names are lowercased
    // to match the engine's case-insensitive `css_lookup`.
    let color_path = src.join(COLOR_FILE);
    let color_text = std::fs::read_to_string(&color_path)
        .map_err(|e| format!("failed to read '{}': {e}", color_path.display()))?;
    let color_text = strip_comments(&color_text);

    let named_colors = extract_css_colors(&color_text)?;
    let legacy_colors = extract_legacy_colors(&color_text);

    // Remaining `tmp == "..."` / `key == "..."` names, minus any already carried (with a value) by
    // the CSS table or the legacy statics — those extras are recognized but have no extractable RGB,
    // so they are membership-only (e.g. `transparent`).
    let name_re = Regex::new(r#"\b(?:tmp|key)\s*==\s*"([^"]+)""#).expect("valid color-name regex");
    let valued: std::collections::HashSet<&str> = named_colors
        .iter()
        .chain(legacy_colors.iter())
        .map(|(n, _)| n.as_str())
        .collect();
    let mut legacy_color_names: Vec<String> = Vec::new();
    for caps in name_re.captures_iter(&color_text) {
        let lower = caps[1].to_ascii_lowercase();
        if !valued.contains(lower.as_str()) {
            legacy_color_names.push(lower);
        }
    }

    Ok(Catalog {
        properties: sorted_dedup(properties),
        color_properties: sorted_dedup(color_properties),
        layout_properties: sorted_dedup(layout_properties),
        native_widget_properties: extract_native_widget_properties(src)?,
        named_colors,
        legacy_colors,
        legacy_color_names: sorted_dedup(legacy_color_names),
    })
}

/// Extract, per native widget class, the OTML tags it dispatches in its **own** `onStyleApply`
/// override — the mechanism by which a C++ subclass adds style properties the base parser knows
/// nothing about (`UITextEdit` → `placeholder`, `UIItem` → `item-id`, `UIGraph` → `capacity`, …).
///
/// Scans every `.cpp` under [`NATIVE_WIDGET_DIRS`] for a `void <Class>::onStyleApply(` definition,
/// takes its body up to the next column-0 `}` (engine style puts every method body at column 0, so
/// this bounds it reliably), and pulls the `node->tag() == "..."` tags out of it — the same dispatch
/// shape the base catalog uses.
///
/// `UIWidget` is skipped: its `onStyleApply` delegates to `parseBaseStyle`, whose tags are already
/// the global catalog, and re-listing them per-widget would say they are *only* valid on `UIWidget`.
///
/// Classes with no dispatched tag are dropped, so the table holds only widgets that genuinely add
/// properties. Missing directories are tolerated (a fork may not ship `src/client`).
fn extract_native_widget_properties(src: &Path) -> Result<Vec<(String, Vec<String>)>, String> {
    let def_re =
        Regex::new(r"\bvoid\s+(UI\w+)::onStyleApply\s*\(").expect("valid onStyleApply regex");
    let tag_re = Regex::new(r#"\btag\s*(?:\(\))?\s*==\s*"([^"]+)""#).expect("valid property regex");

    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for dir in NATIVE_WIDGET_DIRS {
        let dir = src.join(dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!(
                "gen-catalog: note: optional widget dir '{}' not found; skipping.",
                dir.display()
            );
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "cpp"))
            .collect();
        files.sort();

        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let stripped = strip_comments(&text);
            for caps in def_re.captures_iter(&stripped) {
                let class = caps[1].to_string();
                if class == "UIWidget" {
                    continue; // its tags are the base catalog, not a per-widget addition
                }
                // Body: from the end of the signature to the next column-0 `}`.
                let start = caps.get(0).expect("match 0").end();
                let rest = &stripped[start..];
                let end = rest.find("\n}").map_or(rest.len(), |i| i + 1);
                let tags: Vec<String> = tag_re
                    .captures_iter(&rest[..end])
                    .map(|c| c[1].to_string())
                    .collect();
                if tags.is_empty() {
                    continue;
                }
                out.push((class, sorted_dedup(tags)));
            }
        }
    }

    // Merge duplicate class entries (a class split across translation units) and sort by class.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Vec<(String, Vec<String>)> = Vec::new();
    for (class, tags) in out {
        match merged.last_mut() {
            Some((prev, prev_tags)) if *prev == class => {
                prev_tags.extend(tags);
                let taken = std::mem::take(prev_tags);
                *prev_tags = sorted_dedup(taken);
            }
            _ => merged.push((class, tags)),
        }
    }
    if merged.is_empty() {
        return Err(
            "extracted zero native-widget properties — is the widget-source layout as expected?"
                .into(),
        );
    }
    Ok(merged)
}

/// The keys the widget style parser reads directly out of a `layout:` block — the `type` in
/// `layoutType = node->valueAt<std::string>("type", "")`, inside the `node->tag() == "layout"`
/// handler of `uiwidgetbasestyle.cpp`.
///
/// Scoped to that handler (a window from the `"layout"` tag compare to the next tag compare) so an
/// unrelated `valueAt` elsewhere in the style parser is never captured. Extracted rather than
/// hard-coded so a fork that renames the key is picked up by a regenerate.
fn extract_layout_type_keys(src: &Path) -> Result<Vec<String>, String> {
    let path = src.join(BASE_STYLE_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    let stripped = strip_comments(&text);

    // Narrow to the `layout` handler: from its tag compare up to the next one.
    let handler_re =
        Regex::new(r#"\btag\s*(?:\(\))?\s*==\s*"layout"([\s\S]*?)\btag\s*(?:\(\))?\s*==\s*""#)
            .expect("valid layout-handler regex");
    let Some(handler) = handler_re.captures(&stripped) else {
        return Err(
            "could not locate the `tag() == \"layout\"` handler — is the style-dispatch layout as \
             expected?"
                .into(),
        );
    };

    let key_re =
        Regex::new(r#"\bvalueAt\s*<[^>]*>\s*\(\s*"([^"]+)""#).expect("valid valueAt regex");
    let keys: Vec<String> = key_re
        .captures_iter(&handler[1])
        .map(|caps| caps[1].to_string())
        .collect();
    if keys.is_empty() {
        return Err("the `layout` handler yielded no `valueAt` key (expected `type`)".into());
    }
    Ok(keys)
}

/// Extract the OTML property tags whose value the engine parses as a color: a `tag == "..."` compare
/// whose handler statement (up to its terminating `;`, without crossing a `{`/`}` block boundary)
/// parses the whole value as a `Color` — either `node->value<Color>()` or `Color(node->value())`.
/// The `[^;{}]` bound keeps each match inside one statement, so a non-color handler is never
/// mismatched and the brace-bodied `border` / `ttf-stroke` shorthands (whose color is only a
/// sub-token) are excluded. Returns lowercased/verbatim tags (as authored).
fn extract_color_properties(stripped: &str) -> Vec<String> {
    let re = Regex::new(
        r#"\btag\s*(?:\(\))?\s*==\s*"([^"]+)"\s*\)[^;{}]*?(?:value<\s*Color\s*>|Color\s*\(\s*node->value)"#,
    )
    .expect("valid color-property regex");
    re.captures_iter(stripped)
        .map(|caps| caps[1].to_string())
        .collect()
}

/// Extract the legacy engine color statics (`const Color Color::NAME = 0xAABBGGRR;`) as
/// `(lowercased name, packed 0xRRGGBBAA)`, converting the source's AABBGGRR (little-endian channel)
/// literal into RGBA so alpha survives. Sorted by name, de-duped (first wins).
fn extract_legacy_colors(color_text: &str) -> Vec<(String, u32)> {
    let re = Regex::new(r#"const\s+Color\s+Color::(\w+)\s*=\s*0x([0-9A-Fa-f]+)U?\s*;"#)
        .expect("valid legacy-color regex");
    let mut out: Vec<(String, u32)> = Vec::new();
    for caps in re.captures_iter(color_text) {
        let name = caps[1].to_ascii_lowercase();
        // Parse the AABBGGRR literal, then repack as 0xRRGGBBAA.
        if let Ok(abgr) = u32::from_str_radix(&caps[2], 16) {
            let a = (abgr >> 24) & 0xFF;
            let b = (abgr >> 16) & 0xFF;
            let g = (abgr >> 8) & 0xFF;
            let r = abgr & 0xFF;
            out.push((name, (r << 24) | (g << 16) | (b << 8) | a));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Extract the CSS named-color table (`{"name", rgb_to_abgr(0xRRGGBB)}`) as `(lowercased name,
/// 0xRRGGBB)` pairs, sorted by name and de-duped (first value wins on a duplicate name). Kept as its
/// own function so the value parsing can be unit-tested.
fn extract_css_colors(color_text: &str) -> Result<Vec<(String, u32)>, String> {
    let table_re = Regex::new(r#"\{\s*"([A-Za-z]+)"\s*,\s*rgb_to_abgr\(\s*0x([0-9A-Fa-f]+)\s*\)"#)
        .expect("valid table regex");

    let mut named_colors: Vec<(String, u32)> = Vec::new();
    for caps in table_re.captures_iter(color_text) {
        let name = caps[1].to_ascii_lowercase();
        let value = u32::from_str_radix(&caps[2], 16)
            .map_err(|e| format!("bad rgb literal for color '{name}': {e}"))?;
        named_colors.push((name, value));
    }
    if named_colors.is_empty() {
        return Err("extracted zero named colors — is the color table layout as expected?".into());
    }
    named_colors.sort_by(|a, b| a.0.cmp(&b.0));
    named_colors.dedup_by(|a, b| a.0 == b.0);
    Ok(named_colors)
}

/// Sort + de-duplicate for deterministic, idempotent output.
fn sorted_dedup(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

/// Remove C/C++ comments so a commented-out `node->tag() == "..."` never enters the catalog.
/// String and character literals are respected so a `"//"` inside a literal is not mistaken for a
/// comment start. Comment bodies are replaced with a single space to preserve token boundaries.
///
/// The scan works on the raw bytes and copies non-comment bytes verbatim (multi-byte UTF-8 sequences
/// included, byte for byte), decoding back to a `String` only at the end — so non-ASCII content in
/// the source is never mangled. The only bytes we synthesize are ASCII (`' '`), so the result is
/// valid UTF-8 whenever the input was.
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                out.push(b' ');
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' | b'\'' => {
                let quote = b;
                out.push(b);
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    out.push(c);
                    i += 1;
                    if c == b'\\' && i < bytes.len() {
                        out.push(bytes[i]);
                        i += 1;
                    } else if c == quote {
                        break;
                    }
                }
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Every synthesized byte is ASCII and every copied byte came from a valid `&str`, so the buffer
    // is valid UTF-8; `from_utf8_lossy` is a belt-and-suspenders guard that never allocates here.
    String::from_utf8_lossy(&out).into_owned()
}

/// Render the catalog into the committed `catalog.rs` source (with a GENERATED banner).
fn render(catalog: &Catalog) -> String {
    let mut s = String::new();
    s.push_str(
        "//! GENERATED — do not edit by hand. Regenerate with `cargo xtask gen-catalog`.\n\
         //!\n\
         //! This file is generated by `cargo xtask gen-catalog` from the OTClient engine source\n\
         //! (the C++ that parses OTUI at runtime). It is committed to the repo — like the\n\
         //! tree-sitter parser — so the normal build never needs the engine source. The catalog is\n\
         //! regenerated manually when a fork updates the engine; there is no CI drift-guard.\n\
         //!\n\
         //! * [`PROPERTIES`] — the OTML property tag names dispatched by the widget style parsers\n\
         //!   (`parseBaseStyle` / `parseImageStyle` / `parseTextStyle`). Lowercase/kebab, matching\n\
         //!   the engine's exact tag compare.\n\
         //! * [`COLOR_PROPERTIES`] — the subset of property tags whose value the engine parses as a\n\
         //!   color (`node->value<Color>()` / `Color(node->value())`), used to gate named-color\n\
         //!   swatches to genuine color-value positions.\n\
         //! * [`LAYOUT_PROPERTIES`] — the keys read inside a `layout:` block (the union of the\n\
         //!   layout classes' `applyStyle` tags, plus the block's `type`). Disjoint from\n\
         //!   [`PROPERTIES`] — valid only *inside* a `layout:` block.\n\
         //! * [`NATIVE_WIDGET_PROPERTIES`] — the tags each native widget subclass dispatches in its\n\
         //!   own `onStyleApply` override, keyed by `UI` class. Valid only on that class and its\n\
         //!   descendants, so they are resolved through the widget ancestry, not globally.\n\
         //! * [`NAMED_COLORS`] — the CSS named-color table as `(name, 0xRRGGBB)` pairs, lowercased\n\
         //!   to match the engine's case-insensitive lookup. The packed value is the color's RGB.\n\
         //! * [`LEGACY_COLORS`] — the legacy engine color statics as `(name, 0xRRGGBBAA)` pairs\n\
         //!   (alpha preserved), lowercased.\n\
         //! * [`LEGACY_COLOR_NAMES`] — recognized color names with no extractable RGB value (the\n\
         //!   `transparent` alias); membership only.\n\
         //!\n\
         //! A future per-fork variant would add sibling tables here; the single catalog is the\n\
         //! current scope.\n\n",
    );

    s.push_str("/// OTML property tag names recognized by the engine's widget style parsers.\n");
    s.push_str(&render_slice("PROPERTIES", &catalog.properties));
    s.push('\n');
    s.push_str(
        "/// OTML property tags whose value the engine parses as a color (a `value<Color>` / \
         `Color(node->value())` dispatch site).\n",
    );
    s.push_str(&render_slice("COLOR_PROPERTIES", &catalog.color_properties));
    s.push('\n');
    s.push_str(
        "/// Keys the engine reads inside a `layout:` block — the union of the layout classes' \
         `applyStyle`\n\
         /// tags plus the block's own `type` key. Disjoint from [`PROPERTIES`]: these are \
         dispatched by the\n\
         /// layout object, not the widget style parser, so they are only valid *inside* a \
         `layout:` block.\n",
    );
    s.push_str(&render_slice(
        "LAYOUT_PROPERTIES",
        &catalog.layout_properties,
    ));
    s.push('\n');
    s.push_str(
        "/// OTML tags each native widget subclass dispatches in its own `onStyleApply` override: \
         `(UI class,\n\
         /// sorted tags)`. Valid **only** on that class and its descendants — unlike \
         [`PROPERTIES`], which is\n\
         /// global. Resolved through the widget's ancestry; see `widget_resolve`.\n",
    );
    s.push_str(&render_native_widgets(
        "NATIVE_WIDGET_PROPERTIES",
        &catalog.native_widget_properties,
    ));
    s.push('\n');
    s.push_str(
        "/// CSS named colors recognized by the engine's color parser: `(lowercased name, packed \
         0xRRGGBB)`.\n",
    );
    s.push_str(&render_pairs("NAMED_COLORS", &catalog.named_colors, 6));
    s.push('\n');
    s.push_str(
        "/// Legacy engine color statics: `(lowercased name, packed 0xRRGGBBAA)` (alpha \
         preserved).\n",
    );
    s.push_str(&render_pairs("LEGACY_COLORS", &catalog.legacy_colors, 8));
    s.push('\n');
    s.push_str(
        "/// Recognized color names with no extractable RGB value (the `transparent` alias) \
         (lowercased).\n",
    );
    s.push_str(&render_slice(
        "LEGACY_COLOR_NAMES",
        &catalog.legacy_color_names,
    ));

    s
}

/// Render the per-widget native property table as a `&[(&str, &[&str])]`: one row per widget class,
/// each holding that class's own `onStyleApply` tags.
fn render_native_widgets(name: &str, rows: &[(String, Vec<String>)]) -> String {
    let mut s = format!("pub static {name}: &[(&str, &[&str])] = &[\n");
    for (class, tags) in rows {
        s.push_str("    (\"");
        s.push_str(class);
        s.push_str("\", &[");
        for (i, tag) in tags.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push('"');
            s.push_str(tag);
            s.push('"');
        }
        s.push_str("]),\n");
    }
    s.push_str("];\n");
    s
}

fn render_slice(name: &str, values: &[String]) -> String {
    let mut s = format!("pub static {name}: &[&str] = &[\n");
    for v in values {
        s.push_str("    \"");
        s.push_str(v);
        s.push_str("\",\n");
    }
    s.push_str("];\n");
    s
}

/// Render a `&[(&str, u32)]` table with each value as a zero-padded `hex_width`-digit hex literal
/// (6 for `0xRRGGBB`, 8 for `0xRRGGBBAA`).
fn render_pairs(name: &str, values: &[(String, u32)], hex_width: usize) -> String {
    let mut s = format!("pub static {name}: &[(&str, u32)] = &[\n");
    for (n, v) in values {
        s.push_str(&format!(
            "    (\"{n}\", 0x{v:0width$X}),\n",
            width = hex_width
        ));
    }
    s.push_str("];\n");
    s
}

/// The workspace root, derived from this crate's manifest dir (`<root>/xtask`). Robust regardless of
/// the caller's working directory.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .expect("xtask manifest dir has a parent (the workspace root)")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{extract_css_colors, strip_comments};

    #[test]
    fn extracts_css_color_values_sorted_with_rgb() {
        // A miniature `kCss`-shaped table: names + `rgb_to_abgr(0xRRGGBB)` literals. The packed
        // value is the RGB argument verbatim, names are lowercased, and the result is sorted by name.
        let src = r#"
            static constexpr CssPair kCss[] = {
                {"Red",rgb_to_abgr(0xFF0000)}, {"aliceblue",rgb_to_abgr(0xF0F8FF)},
                {"black",rgb_to_abgr(0x000000)},
            };
        "#;
        let colors = extract_css_colors(src).expect("parses");
        assert_eq!(
            colors,
            vec![
                ("aliceblue".to_owned(), 0x00F0_F8FF),
                ("black".to_owned(), 0x0000_0000),
                ("red".to_owned(), 0x00FF_0000),
            ]
        );
    }

    #[test]
    fn empty_color_table_is_an_error() {
        assert!(extract_css_colors("no table here").is_err());
    }

    #[test]
    fn extracts_only_color_typed_properties() {
        // A miniature style-dispatch chain: two color-parsed tags (via `value<Color>` and
        // `Color(node->value())`), one int tag, and a brace-bodied `border` whose color is only a
        // sub-token. Only the two whole-value color tags are captured.
        let src = r#"
            if (node->tag() == "color")
                setColor(node->value<Color>());
            else if (node->tag() == "width")
                setWidth(node->value<int>());
            else if (node->tag() == "ttf-stroke-color")
                ttfStrokeColor = Color(node->value());
            else if (node->tag() == "border") {
                Color c = stdext::safe_cast<Color>(token);
            }
        "#;
        let mut got = super::extract_color_properties(src);
        got.sort();
        assert_eq!(got, vec!["color".to_owned(), "ttf-stroke-color".to_owned()]);
    }

    #[test]
    fn extracts_legacy_color_statics_as_rgba() {
        // AABBGGRR literal repacked to RGBA; alpha preserved (opaque and fully transparent).
        let src = "const Color Color::red = 0xff0000ffU;\n\
                   const Color Color::alpha = 0x00000000U;\n\
                   const Color Color::teal = 0xffffff00U;\n";
        let got = super::extract_legacy_colors(src);
        assert_eq!(
            got,
            vec![
                ("alpha".to_owned(), 0x0000_0000), // fully transparent
                ("red".to_owned(), 0xFF00_00FF),   // opaque red
                ("teal".to_owned(), 0x00FF_FFFF),  // engine teal is cyan, opaque
            ]
        );
    }

    #[test]
    fn strips_line_and_block_comments_but_keeps_code() {
        let src = "int a; // a comment\nb == \"x\"; /* block */ c;\n";
        let out = strip_comments(src);
        assert!(out.contains("int a;"));
        assert!(!out.contains("a comment"));
        assert!(!out.contains("block"));
        // The string literal survives; the block comment collapses to a boundary space.
        assert!(out.contains("b == \"x\";"));
        assert!(out.contains("c;"));
    }

    #[test]
    fn a_double_slash_inside_a_string_literal_is_not_a_comment() {
        let src = "tag == \"http://x\"; // real comment\n";
        let out = strip_comments(src);
        assert!(out.contains("\"http://x\""));
        assert!(!out.contains("real comment"));
    }

    #[test]
    fn non_ascii_bytes_are_preserved_verbatim() {
        // A comment with multi-byte UTF-8 is removed; multi-byte content in code/strings survives
        // byte-for-byte (a naive `byte as char` cast would mangle these).
        let src = "x == \"café\"; // córrego\ny;\n";
        let out = strip_comments(src);
        assert!(
            out.contains("\"café\""),
            "multibyte string literal preserved: {out}"
        );
        assert!(!out.contains("córrego"));
        assert!(out.contains("y;"));
        // Output stays valid UTF-8 (would have panicked/garbled under the old cast).
        assert!(out.is_char_boundary(out.len()));
    }
}
