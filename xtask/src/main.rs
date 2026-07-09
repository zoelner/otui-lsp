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
    named_colors: Vec<String>,
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
        "gen-catalog: wrote {} properties and {} named colors to {}",
        catalog.properties.len(),
        catalog.named_colors.len(),
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

    // Named colors: the CSS table entries `{"name", rgb_to_abgr(0x...)}` plus the legacy engine
    // color names and the `transparent` alias compared as `tmp == "..."` / `key == "..."`. Names are
    // lowercased to match the engine's case-insensitive `css_lookup`.
    let color_path = src.join(COLOR_FILE);
    let color_text = std::fs::read_to_string(&color_path)
        .map_err(|e| format!("failed to read '{}': {e}", color_path.display()))?;
    let color_text = strip_comments(&color_text);

    let table_re =
        Regex::new(r#"\{\s*"([A-Za-z]+)"\s*,\s*rgb_to_abgr"#).expect("valid table regex");
    let name_re = Regex::new(r#"\b(?:tmp|key)\s*==\s*"([^"]+)""#).expect("valid color-name regex");

    let mut named_colors: Vec<String> = Vec::new();
    for caps in table_re.captures_iter(&color_text) {
        named_colors.push(caps[1].to_ascii_lowercase());
    }
    for caps in name_re.captures_iter(&color_text) {
        named_colors.push(caps[1].to_ascii_lowercase());
    }
    if named_colors.is_empty() {
        return Err("extracted zero named colors — is the color table layout as expected?".into());
    }

    Ok(Catalog {
        properties: sorted_dedup(properties),
        named_colors: sorted_dedup(named_colors),
    })
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
fn strip_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
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
                out.push(' ');
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'"' | b'\'' => {
                let quote = b;
                out.push(b as char);
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    out.push(c as char);
                    i += 1;
                    if c == b'\\' && i < bytes.len() {
                        out.push(bytes[i] as char);
                        i += 1;
                    } else if c == quote {
                        break;
                    }
                }
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
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
         //! * [`NAMED_COLORS`] — the CSS named-color table plus the legacy engine color names and\n\
         //!   the `transparent` alias, lowercased to match the engine's case-insensitive lookup.\n\
         //!\n\
         //! A future per-fork variant would add sibling tables here; the single catalog is the\n\
         //! current scope.\n\n",
    );

    s.push_str("/// OTML property tag names recognized by the engine's widget style parsers.\n");
    s.push_str(&render_slice("PROPERTIES", &catalog.properties));
    s.push('\n');
    s.push_str("/// Named colors recognized by the engine's color parser (lowercased).\n");
    s.push_str(&render_slice("NAMED_COLORS", &catalog.named_colors));

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

/// The workspace root, derived from this crate's manifest dir (`<root>/xtask`). Robust regardless of
/// the caller's working directory.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .expect("xtask manifest dir has a parent (the workspace root)")
        .to_path_buf()
}
