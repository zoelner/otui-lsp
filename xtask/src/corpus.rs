//! Fidelity harness: run the widget-aware diagnostics over a real engine corpus and report each
//! `unknown-property` finding **with the widget it sits on**, so a genuine dead property (one the
//! engine reads nowhere) can be told apart from a gap in the LSP's catalog.
//!
//! Usage: `cargo xtask corpus --src <engine-source-root>`
use otui_core::diagnostics::{analyze_with_widgets, WidgetContext};
use otui_core::lua_widgets::{scan_widgets, LuaWidgetIndex};
use otui_core::style_index::{extract_style_defs, StyleIndex};
use otui_core::syntax::SyntaxTree;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Run the harness over the engine tree at `root` and print the report to stdout.
pub fn run(root: &Path) {
    let (otui, lua) = (files(root, "otui"), files(root, "lua"));

    let mut styles = StyleIndex::new();
    let mut luas = LuaWidgetIndex::new();
    for f in &otui {
        if let Ok(s) = std::fs::read_to_string(f) {
            if let Some(t) = SyntaxTree::parse(&s) {
                styles.set_document(f.display().to_string(), extract_style_defs(&t));
            }
        }
    }
    for f in &lua {
        if let Ok(s) = std::fs::read_to_string(f) {
            luas.set_document(f.display().to_string(), scan_widgets(&s));
        }
    }
    let ctx = WidgetContext {
        styles: &styles,
        lua: &luas,
    };

    let mut pairs: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut by_code: BTreeMap<&str, usize> = BTreeMap::new();
    for f in &otui {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        for d in analyze_with_widgets(&src, &ctx) {
            *by_code.entry(d.code).or_default() += 1;
            if d.code != "unknown-property" {
                continue;
            }
            let prop = src[d.span.start..d.span.end].to_string();
            let w = enclosing_widget(&src, d.span.start).unwrap_or_else(|| "?".into());
            *pairs.entry((prop, w)).or_default() += 1;
        }
    }
    println!(
        "{} .otui | {} .lua | by code: {by_code:?}\n",
        otui.len(),
        lua.len()
    );
    let mut v: Vec<_> = pairs.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    println!("   n  property                     widget");
    for ((prop, w), c) in v.iter().take(30) {
        println!("{c:>4}  {prop:<28} {w}");
    }
}

/// The nearest enclosing widget — mirroring the rule `diagnostics` resolves a property against: a
/// `container`'s `tag`, or a `style_header`'s **declared name** (falling back to its `base`).
///
/// The name, not the base: `diagnostics` seeds a style body from the declared name so a `__class:`
/// re-root in that body applies. Reporting the base here would label a finding inside
/// `SpinBox < TextEdit` as `TextEdit` — misattributing it to the wrong widget, which is exactly the
/// column this report exists to get right.
fn enclosing_widget(src: &str, offset: usize) -> Option<String> {
    let tree = SyntaxTree::parse(src)?;
    let mut node = tree.root().descendant_for_byte_range(offset, offset)?;
    loop {
        let fields: &[&str] = match node.kind() {
            "container" => &["tag"],
            "style_header" => &["name", "base"],
            _ => &[],
        };
        for field in fields {
            if let Some(n) = node.child_by_field_name(field) {
                return Some(src[n.start_byte()..n.end_byte()].trim().to_string());
            }
        }
        node = node.parent()?;
    }
}

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn go(d: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                go(&p, ext, out)
            } else if p.extension().is_some_and(|x| x == ext) {
                out.push(p)
            }
        }
    }
    go(dir, ext, &mut out);
    out
}
