//! Fidelity harness: run the widget-aware diagnostics over a real engine corpus and report each
//! `unknown-property` finding **with the widget it sits on**, so a genuine dead property (one the
//! engine reads nowhere) can be told apart from a gap in the LSP's catalog. Also counts
//! `missing-asset` (a path property whose target does not resolve to a file on disk) over the same
//! tree, via the exact resolution the server uses (`resolve_asset_candidates`) — a diagnostic that
//! is I/O-driven, unlike everything else `analyze_with_widgets` reports, so it is layered on top
//! here rather than coming out of that pass.
//!
//! Usage: `cargo xtask corpus --src <engine-source-root>`
use otui_core::diagnostics::{analyze_with_widgets, WidgetContext};
use otui_core::links::{
    document_links, is_asset_sentinel_value, is_runtime_variable_path, resolve_asset_candidates,
};
use otui_core::lua_widgets::{scan_widgets, LuaWidgetIndex};
use otui_core::style_index::{extract_style_defs, StyleIndex};
use otui_core::syntax::SyntaxTree;
// `detect_client_roots`/`otpkg_present_under` do real filesystem I/O (walking ancestor directories,
// recursively scanning for `.otpkg`), so — per the workspace's hard rule that `otui-core` stays
// I/O-free — they live in the server crate, not `otui-core`, even though everything else this module
// needs for asset resolution now does (`otui_core::links`, above). One source of truth for both
// halves of path resolution, not a second copy in `xtask`.
use otui_lsp_server::{detect_client_roots, otpkg_present_under};
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
            let w = enclosing_widget(&src, d.span.start, &styles).unwrap_or_else(|| "?".into());
            *pairs.entry((prop, w)).or_default() += 1;
        }
    }
    let (missing_asset_count, missing_asset_examples) = missing_asset_findings(&otui, root);
    by_code.insert("missing-asset", missing_asset_count);

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

    if !missing_asset_examples.is_empty() {
        println!(
            "\nmissing-asset: {missing_asset_count} finding(s); first {} for manual inspection:",
            missing_asset_examples.len()
        );
        for (file, path) in &missing_asset_examples {
            println!("  {}: {path}", file.display());
        }
    }
}

/// Count `missing-asset` findings over `otui` (spec-parallel to the `unknown-property` pass above,
/// but I/O-driven — not something `analyze_with_widgets` can report — so it is a separate walk
/// here): for every path-valued property ([`document_links`]) whose value does not contain `$` (an
/// OTML runtime variable — see the identical guard in `otui_lsp_server::missing_asset_diagnostics`)
/// and does not resolve to an existing file via [`resolve_asset_candidates`], using the *detected*
/// OTClient install root(s) ([`detect_client_roots`]) — `root` of the corpus tree standing in for
/// the single workspace root; each file's own directory for relative paths.
///
/// Mirrors `missing_asset_diagnostics`'s guards exactly, not a re-guessed subset: no detected client
/// root anywhere suppresses the whole tree (Finding 2 — there is nothing to trust as a data root),
/// and a mounted `*.otpkg` anywhere under a detected root suppresses the whole tree too (Finding 3 —
/// an asset shipped inside one is invisible to `is_file()`). This keeps the count from silently
/// drifting from what the real diagnostic would report over the same tree.
///
/// Returns the total count plus up to 40 `(file, raw path)` examples, so a human can spot-check
/// what the rule actually flags rather than trusting the count alone.
fn missing_asset_findings(otui: &[PathBuf], root: &Path) -> (usize, Vec<(PathBuf, String)>) {
    let workspace_roots = [root.to_path_buf()];
    let client_roots = detect_client_roots(Some(root), &workspace_roots);
    if client_roots.is_empty() {
        return (0, Vec::new());
    }
    if client_roots.iter().any(|r| otpkg_present_under(r)) {
        return (0, Vec::new());
    }

    let mut count = 0usize;
    let mut examples = Vec::new();
    for f in otui {
        let Ok(src) = std::fs::read_to_string(f) else {
            continue;
        };
        let Some(doc_dir) = f.parent() else {
            continue;
        };
        for link in document_links(&src) {
            if is_runtime_variable_path(&link.path) || is_asset_sentinel_value(&link.path) {
                continue;
            }
            let resolved = resolve_asset_candidates(&link.path, doc_dir, &client_roots)
                .into_iter()
                .any(|c| c.is_file());
            if resolved {
                continue;
            }
            count += 1;
            if examples.len() < 40 {
                examples.push((f.clone(), link.path.clone()));
            }
        }
    }
    (count, examples)
}

/// The nearest enclosing widget, resolved by **exactly** the rule `diagnostics` uses to decide what a
/// property is checked against: a `container`'s `tag`; a `style_header`'s declared **name** when the
/// style index knows it, else its `base`.
///
/// The name, not the base: `diagnostics` seeds a style body from the declared name so a `__class:`
/// re-root in that body applies. Reporting the base would label a finding inside `SpinBox < TextEdit`
/// as `TextEdit` — misattributing it to the wrong widget, which is the one column this report exists
/// to get right. And the index check, not a bare preference for the name: an un-indexed header falls
/// back to the base in `diagnostics`, so mirroring that keeps the label honest about which widget the
/// property was actually judged against.
fn enclosing_widget(src: &str, offset: usize, styles: &StyleIndex) -> Option<String> {
    let tree = SyntaxTree::parse(src)?;
    let mut node = tree.root().descendant_for_byte_range(offset, offset)?;
    loop {
        let text = |field: &str| {
            node.child_by_field_name(field)
                .map(|n| src[n.start_byte()..n.end_byte()].trim().to_string())
        };
        match node.kind() {
            "container" => {
                if let Some(tag) = text("tag") {
                    return Some(tag);
                }
            }
            "style_header" => {
                let name = text("name").filter(|n| !styles.lookup(n).is_empty());
                if let Some(w) = name.or_else(|| text("base")) {
                    return Some(w);
                }
            }
            _ => {}
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
