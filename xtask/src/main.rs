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
    ("src/framework/ui/uiwidgetbasestyle.cpp", true),
    ("src/framework/ui/uiwidgetimage.cpp", false),
    ("src/framework/ui/uiwidgettext.cpp", false),
];

/// Relative path (under the engine-source root) of the CSS named-color table + legacy color names.
const COLOR_FILE: &str = "src/framework/util/color.cpp";

/// Output path (relative to the workspace root) of the committed, generated catalog.
const OUTPUT_REL: &str = "crates/otui-core/src/catalog.rs";

/// The extracted, sorted, de-duped catalog ready to be rendered to Rust source.
struct Catalog {
    properties: Vec<String>,
    /// The CSS named-color table: `(lowercased name, packed 0xRRGGBB)`, sorted by name. The value is
    /// captured from the `rgb_to_abgr(0xRRGGBB)` literal in the engine's `kCss` table.
    named_colors: Vec<(String, u32)>,
    /// Legacy engine color names (the `tmp == "..."` / `key == "..."` statics, e.g. `alpha`,
    /// `darkPink`, `transparent`) that are **not** in the CSS table, so carry no extractable RGB
    /// value — kept names-only for `is_named_color` membership. Lowercased, sorted.
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
        "gen-catalog: wrote {} properties, {} named colors and {} legacy color names to {}",
        catalog.properties.len(),
        catalog.named_colors.len(),
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
    }
    if properties.is_empty() {
        return Err(
            "extracted zero property tags — is the engine source layout as expected?".into(),
        );
    }

    // Named colors: the CSS table entries `{"name", rgb_to_abgr(0xRRGGBB)}` (name + value) plus the
    // legacy engine color names and the `transparent` alias compared as `tmp == "..."` /
    // `key == "..."` (names only — the legacy statics carry an AABBGGRR literal we do not extract).
    // Names are lowercased to match the engine's case-insensitive `css_lookup`.
    let color_path = src.join(COLOR_FILE);
    let color_text = std::fs::read_to_string(&color_path)
        .map_err(|e| format!("failed to read '{}': {e}", color_path.display()))?;
    let color_text = strip_comments(&color_text);

    let named_colors = extract_css_colors(&color_text)?;

    // Legacy `tmp == "..."` / `key == "..."` names, minus any already carried (with a value) by the
    // CSS table — those extras have no CSS-table RGB, so they are membership-only.
    let name_re = Regex::new(r#"\b(?:tmp|key)\s*==\s*"([^"]+)""#).expect("valid color-name regex");
    let css_names: std::collections::HashSet<&str> =
        named_colors.iter().map(|(n, _)| n.as_str()).collect();
    let mut legacy_color_names: Vec<String> = Vec::new();
    for caps in name_re.captures_iter(&color_text) {
        let lower = caps[1].to_ascii_lowercase();
        if !css_names.contains(lower.as_str()) {
            legacy_color_names.push(lower);
        }
    }

    Ok(Catalog {
        properties: sorted_dedup(properties),
        named_colors,
        legacy_color_names: sorted_dedup(legacy_color_names),
    })
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
         //! * [`NAMED_COLORS`] — the CSS named-color table as `(name, 0xRRGGBB)` pairs, lowercased\n\
         //!   to match the engine's case-insensitive lookup. The packed value is the color's RGB.\n\
         //! * [`LEGACY_COLOR_NAMES`] — the legacy engine color names and the `transparent` alias\n\
         //!   that are not in the CSS table, so carry no extractable RGB value (membership only).\n\
         //!\n\
         //! A future per-fork variant would add sibling tables here; the single catalog is the\n\
         //! current scope.\n\n",
    );

    s.push_str("/// OTML property tag names recognized by the engine's widget style parsers.\n");
    s.push_str(&render_slice("PROPERTIES", &catalog.properties));
    s.push('\n');
    s.push_str(
        "/// CSS named colors recognized by the engine's color parser: `(lowercased name, packed \
         0xRRGGBB)`.\n",
    );
    s.push_str(&render_pairs("NAMED_COLORS", &catalog.named_colors));
    s.push('\n');
    s.push_str(
        "/// Legacy engine color names (and the `transparent` alias) with no CSS-table RGB value \
         (lowercased).\n",
    );
    s.push_str(&render_slice(
        "LEGACY_COLOR_NAMES",
        &catalog.legacy_color_names,
    ));

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

fn render_pairs(name: &str, values: &[(String, u32)]) -> String {
    let mut s = format!("pub static {name}: &[(&str, u32)] = &[\n");
    for (n, v) in values {
        s.push_str(&format!("    (\"{n}\", 0x{v:06X}),\n"));
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
