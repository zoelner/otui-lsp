//! Locating asset **path** values in a document for the LSP `documentLink` feature.
//!
//! Walks the CST for the value of every `property` whose key is a file-path-valued OTUI tag
//! ([`schema::PATH_PROPERTIES`] — primarily `image-source`) and reports the value token's byte span
//! plus the raw path string. This is a **pure finder**: byte offsets only, no filesystem, no
//! `lsp-types`. Resolving a path to an actual file on disk — and whether the target exists — is I/O
//! and belongs in the server, not here.
//!
//! ## What counts as a link
//!
//! Only real `property` nodes are considered — the generic `key: value` form the grammar tags as
//! `property`. An `id_property`, an anchor/event/alias/expr property, a bare container tag, or a
//! style header never contributes a link, even if it happens to share a spelling. The value must be
//! non-empty after trimming; the reported span is tightened to cover exactly the trimmed path text
//! (so the editor underlines just the path, not any surrounding whitespace the value token spans).

use std::path::{Path, PathBuf};

use lang_api::ByteSpan;
use tree_sitter::Node;

use crate::schema;
use crate::syntax::SyntaxTree;

/// A path-valued property occurrence: the byte span of the value token (tightened to the trimmed
/// path text) and the raw path string it carries. The span is a byte span into the document this was
/// scanned from; the server maps it to an LSP range and resolves `path` against the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRef {
    /// The byte span of the path value token in the source (trimmed to the path text).
    pub span: ByteSpan,
    /// The raw path string (the value text, trimmed). Not resolved — the server resolves it.
    pub path: String,
}

/// Find every file-path-valued property value in `source` (LSP `documentLink`). For each `property`
/// whose key is in [`schema::PATH_PROPERTIES`], the value token's trimmed span and raw path text are
/// returned. Non-path properties (`id: x`, `text: y`), non-`property` nodes, and properties with an
/// empty value are ignored. Returns an empty vector when the source cannot be parsed.
///
/// Parses `source` itself. A caller that already holds a [`SyntaxTree`] for the same `source` (e.g.
/// one computed alongside diagnostics) should call [`document_links_from_tree`] instead, so the
/// document is not parsed twice for one request.
#[must_use]
pub fn document_links(source: &str) -> Vec<PathRef> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    document_links_from_tree(source, &tree)
}

/// Like [`document_links`], but over an already-parsed `tree` instead of parsing `source` again —
/// for a caller that needs a second source-derived pass (asset-link extraction) over a document it
/// has already parsed for another purpose (diagnostics), so the two passes share one parse.
#[must_use]
pub fn document_links_from_tree(source: &str, tree: &SyntaxTree) -> Vec<PathRef> {
    let mut out = Vec::new();
    collect(tree.root(), source, &mut out);
    out
}

/// Point-locate the file-path-valued property **value** the cursor is on: if `offset` falls inside
/// the trimmed path text of a `property` whose key is in [`schema::PATH_PROPERTIES`], return that
/// property's [`PathRef`]; otherwise `None`.
///
/// This complements [`document_links`]'s bulk sweep with a point query — "what asset, if any, is
/// under the cursor?" (e.g. to drive a sprite-preview hover). A cursor on the **key**
/// (`image-source`) is deliberately not a hit here — that position is
/// [`property_hover_at`](crate::property_hover::property_hover_at)'s job — nor is a cursor on the
/// value of a non-path property, or anywhere outside a property.
///
/// Spans are half-open `[start, end)`, matching the rest of this crate's locators (see
/// `navigation::base_reference_at`): an offset exactly at the end of the path text is not inside it.
#[must_use]
pub fn asset_ref_at(source: &str, offset: usize) -> Option<PathRef> {
    let tree = SyntaxTree::parse(source)?;
    let start = tree.root().descendant_for_byte_range(offset, offset)?;
    let mut node = start;
    let property = loop {
        if node.kind() == "property" {
            break node;
        }
        node = node.parent()?;
    };
    let path_ref = path_ref(property, source)?;
    if path_ref.span.start <= offset && offset < path_ref.span.end {
        Some(path_ref)
    } else {
        None
    }
}

/// Pre-order walk emitting a [`PathRef`] for every path-valued `property` under `node`.
fn collect(node: Node<'_>, source: &str, out: &mut Vec<PathRef>) {
    if node.kind() == "property" {
        if let Some(path_ref) = path_ref(node, source) {
            out.push(path_ref);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// Build the [`PathRef`] for `property` when its key is a path-valued tag and its value is a
/// non-empty (after trimming) path; `None` otherwise. The key compare is exact (case sensitive),
/// matching the engine's `node->tag() == "..."` dispatch. The returned span is tightened to the
/// trimmed value text so it underlines exactly the path.
fn path_ref(property: Node<'_>, source: &str) -> Option<PathRef> {
    let key = property.child_by_field_name("key")?;
    let key_text = &source[key.start_byte()..key.end_byte()];
    if !schema::PATH_PROPERTIES.contains(&key_text) {
        return None;
    }
    let value = property.child_by_field_name("value")?;
    let span = SyntaxTree::span_of(value);
    let raw = &source[span.start..span.end];
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Tighten the span onto the trimmed text: the value token may span leading/trailing whitespace
    // the engine ignores, and we want the link range to cover just the path.
    let lead = raw.len() - raw.trim_start().len();
    let start = span.start + lead;
    let end = start + trimmed.len();
    Some(PathRef {
        span: ByteSpan::new(start, end),
        path: trimmed.to_owned(),
    })
}

/// The top-level directories OTClient overlays onto one virtual root filesystem at startup, in the
/// engine's search order (first match wins). Verified against `init.lua`:
/// ```lua
/// g_resources.addSearchPath(g_resources.getWorkDir() .. 'data', true)     -- mounted, then...
/// g_resources.addSearchPath(g_resources.getWorkDir() .. 'modules', true)  -- ...pushed in front...
/// g_resources.addSearchPath(g_resources.getWorkDir() .. 'mods', true)     -- ...of this.
/// ```
/// `ResourceManager::addSearchPath(path, pushFront)` (`resourcemanager.cpp`) does
/// `pushFront ? m_searchPaths.push_front(...) : push_back(...)` — so each successive call *in front*
/// of the previous, making the actual lookup order (first match wins) `mods`, then `modules`, then
/// `data`. A `/`-rooted OTUI asset path (`UIWidget::parseImageStyle` → `g_textures.getTexture` →
/// `TextureManager::getTexture` → `g_resources.resolvePath`, all in the engine source) is resolved
/// against this overlay, **not** against a single flat "data root" — so `/game_x/images/y` can
/// legitimately resolve inside `modules/game_x/images/y.png`, a module's own asset folder, with no
/// `data/` involved at all.
///
/// **The install root itself is also always mounted**, ahead of everything `init.lua` adds — a
/// candidate a CodeRabbit review of this crate (PR #51, Finding 2) claimed did not exist, reasoning
/// only from `init.lua`'s three `addSearchPath` calls above. That reasoning missed the earlier,
/// unconditional C++ mount: `main()` calls `g_resources.discoverWorkDir("init.lua")`
/// (`src/main.cpp`) before any Lua ever runs; `ResourceManager::discoverWorkDir`
/// (`resourcemanager.cpp`) `PHYSFS_mount`s each install-root candidate directory at the virtual root
/// to test for `init.lua`, and on the directory that has it, **breaks out of the loop without ever
/// unmounting it** — so the install root stays mounted (at the point in `m_searchPaths` where
/// `discoverWorkDir` added it, ahead of `data`/`modules`/`mods`, which are pushed in front of it
/// later) for the rest of the session. Verified directly against the vendored `otclient` source, not
/// just `init.lua`: a real, autoloaded, shipped module corpus (`game_proficiency`, `game_healthcircle`,
/// `game_cyclopedia`, `game_inventory` — 35 distinct `image-source`/`icon` paths) writes
/// `/data/…`/`/modules/…`-prefixed asset paths that resolve **only** via this bare-root mount (never
/// via the `mods`/`modules`/`data` overlay, which would require an impossible doubly-nested
/// `data/data/…`) — concrete, on-disk proof the bare root is a real, load-bearing search path, not a
/// hypothetical one. [`resolve_asset_candidates`] must therefore keep probing it.
pub const ASSET_MOUNT_DIRS: [&str; 3] = ["mods", "modules", "data"];

/// Compute the candidate filesystem paths a raw OTUI asset path could resolve to, **without**
/// touching the filesystem (no existence check — that stays with the caller).
///
/// This mirrors OTClient's path model: the real virtual filesystem is an overlay of the install root
/// itself plus `mods/`, `modules/` and `data/` (see [`ASSET_MOUNT_DIRS`]'s doc comment for the full
/// `main.cpp`/`resourcemanager.cpp` mount trace, including why the bare root is always one of the
/// mounted search paths) — never a single flat "data root", and never *only* the three named
/// overlays. Callers choose what `roots` means: the `missing-asset` diagnostic passes only confirmed
/// install roots (so it can stay silent when detection fails), while `document_link`/the hover
/// preview fall back to raw workspace roots when no client root is detected (best-effort — an absent
/// link or preview is harmless, unlike a Warning). Either way the resolution shape is the same: the
/// engine does not distinguish a "confirmed" root from a "guessed" one once it has one.
///
/// * A **`/`-rooted** path is an OTClient "absolute" path — relative to the mounted virtual root, not
///   the OS root or any single "data" directory. The leading `/` is stripped and the remainder is
///   joined directly onto each root (the bare-root mount — covering both a workspace folder that
///   already points *at* one of the mounted directories, and the real, always-mounted install root;
///   see [`ASSET_MOUNT_DIRS`]'s doc comment), **and** onto each of `root/mods`, `root/modules`,
///   `root/data` — the engine's actual overlay. With no `roots` at all a `/`-rooted path yields no
///   candidates offline.
/// * Any **other** (relative) path is resolved against the current document's directory (approximating
///   the engine's "resolve relative to the currently executing script" rule).
/// * **Extensionless** paths get a `.png` variant probed first: OTClient's texture loader appends
///   `.png` to a source with no extension, and OTUI authors almost always omit it
///   (`image-source: /images/ui/button` → `button.png` on disk). Without this the link would never
///   resolve for the overwhelmingly common extensionless form. See [`asset_probe_variants`].
///
/// Returns the candidates in probe order; the caller keeps the first that exists.
#[must_use]
pub fn resolve_asset_candidates(raw: &str, doc_dir: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let path = raw.trim();
    if path.is_empty() {
        return Vec::new();
    }
    let bases: Vec<PathBuf> = if let Some(rest) = path.strip_prefix('/') {
        // OTClient "absolute" = relative to the mounted virtual root; approximate the mount as each
        // root directly (the install root is always itself a mounted search path — see
        // `ASSET_MOUNT_DIRS`'s doc comment), plus each of its conventional `mods`/`modules`/`data`
        // subdirectories (the engine's real overlay). Strip the leading `/` so `join` does not
        // discard the root.
        let mut bases = Vec::new();
        for root in roots {
            bases.push(root.join(rest));
            for mount in ASSET_MOUNT_DIRS {
                bases.push(root.join(mount).join(rest));
            }
        }
        bases
    } else {
        vec![doc_dir.join(path)]
    };
    bases.into_iter().flat_map(asset_probe_variants).collect()
}

/// A path value containing `$` is an OTML runtime-resolved variable (`otmlparser.cpp` substitutes
/// `$name` from an alias/Lua-field map when the *document* is parsed at runtime), not a literal
/// filesystem path — the server has no way to know what it resolves to, so it must never be
/// diagnosed as missing.
#[must_use]
pub fn is_runtime_variable_path(path: &str) -> bool {
    path.contains('$')
}

/// A path property value that is not actually a path at all: an explicit "no image" sentinel, not
/// a broken reference. Verified against `UIWidget::parseImageStyle` (`uiwidgetimage.cpp`):
/// ```cpp
/// if (value == "" || value == "none") { setImageSource("", base64); } // else resolve_path(value, ...)
/// ```
/// — `image-source: none` and `image-source: ""` are the documented ways to clear an inherited
/// image, found in the real corpus (`game_cyclopedia/tab/house/house.otui`,
/// `client_options/styles/controls/keybinds.otui`). The engine's OTML parser normalizes a scalar
/// value by trimming, then stripping one layer of matching `"…"`/`'…'` quotes, before this
/// comparison runs (`otmlparser.cpp`'s `normalizeValue`/`stripQuotes`) — this mirrors that so the
/// literal token text `""` (quotes included, as captured by [`PathRef`]) is recognized too.
///
/// Deliberately applied to **every** `otui_core::schema::PATH_PROPERTIES` key, not just `image-source` (the
/// only one actually special-cased in the engine source above): `icon`/`icon-source` have no
/// matching short-circuit in `uiwidgetbasestyle.cpp` (they call `resolve_path` unconditionally), so
/// treating their `""`/`none` the same way is a deliberately conservative generalization, not a
/// verified engine behavior — false-negative (a genuinely broken `icon: none` typo goes unflagged)
/// is the safe side to err on for a diagnostic, not false-positive (flagging an intentional "no
/// icon").
///
/// Also recognizes an inline `base64:<blob>` value as "not a path": `UIWidget::parseImageStyle`
/// (`uiwidgetimage.cpp`) splits `image-source` on `:` and, when the first segment is exactly
/// `"base64"` and a second segment exists, decodes the rest with `g_crypt.base64Decode()` and loads
/// the texture straight from the decoded bytes (`setImageSource(.., base64=true)` →
/// `g_textures.loadTexture(stream)`) — **the filesystem is never touched**, so there is no path to
/// probe and nothing that can ever be "missing" on disk. `base64:` alone (no second segment) is not
/// this case — the engine's own `split.size() > 1` guard requires content after the colon — and
/// falls through to ordinary path resolution, matching the engine's `value = split[0]` behavior for
/// that shape.
#[must_use]
pub fn is_asset_sentinel_value(raw: &str) -> bool {
    let trimmed = raw.trim();
    let unquoted = strip_matching_quotes(trimmed);
    if unquoted.is_empty() || unquoted == "none" {
        return true;
    }
    if let Some((scheme, rest)) = unquoted.split_once(':') {
        if scheme == "base64" && !rest.is_empty() {
            return true;
        }
    }
    false
}

/// Strip one layer of matching `"…"` or `'…'` quotes from `s`, mirroring `otmlparser.cpp`'s
/// `stripQuotes` (the real OTML scalar-value normalization). `s` unchanged if it is not quoted.
fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Expand one resolved base path into the on-disk variants to probe, mirroring OTClient's texture
/// loader exactly: `ResourceManager::guessFilePath(filename, "png")` (`resourcemanager.cpp`) is
///
/// ```cpp
/// if (isFileType(filename, type)) return filename;   // filename.ends_with(".png")
/// return filename + "." + type;                      // else: literal string concatenation
/// ```
///
/// — **not** "replace the extension". A path is probed as-is only when its filename already ends in
/// `.png` (case-sensitive, matching `ends_with`); every other path — extensionless (`.../button`) or
/// carrying some *other* extension (`.../icon.small`, `.../icon.9`) — gets `.png` concatenated onto
/// the full literal name first (`button.png`, `icon.small.png`, `icon.9.png`), then the literal
/// itself as a harmless fallback. Keying off `Path::extension().is_some()` wrongly treats any dotted
/// stem as "already has its final extension" and would skip the `.png` probe entirely — a false
/// positive for the common `name.<variant>` sprite-sheet naming shape.
fn asset_probe_variants(base: PathBuf) -> Vec<PathBuf> {
    let ends_with_png = base
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".png"));
    if ends_with_png {
        vec![base]
    } else {
        let mut with_png = base.clone().into_os_string();
        with_png.push(".png");
        vec![PathBuf::from(with_png), base]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent) — the house cursor
    /// helper shared across this crate's `*_at` locator tests.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// The `(text slice, path)` for each link found, for readable assertions.
    fn links_with_text(source: &str) -> Vec<(&str, String)> {
        document_links(source)
            .into_iter()
            .map(|r| (&source[r.span.start..r.span.end], r.path))
            .collect()
    }

    #[test]
    fn finds_image_source_value_with_span() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        let links = document_links(source);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].path, "/images/ui/window");
        // The span covers exactly the path text.
        assert_eq!(
            &source[links[0].span.start..links[0].span.end],
            "/images/ui/window"
        );
        assert_eq!(links[0].span.start, source.find("/images").unwrap());
    }

    #[test]
    fn finds_icon_and_icon_source_paths() {
        let source =
            "Button\n  icon: /images/icons/ok.png\nOther\n  icon-source: things/sprites.png\n";
        let found = links_with_text(source);
        assert_eq!(
            found,
            vec![
                ("/images/icons/ok.png", "/images/icons/ok.png".to_owned()),
                ("things/sprites.png", "things/sprites.png".to_owned()),
            ]
        );
    }

    #[test]
    fn ignores_non_path_properties() {
        // `id:` is an `id_property`, `text:` is a non-path `property`; neither is a link.
        let source = "Panel\n  id: main\n  text: Hello World\n  color: red\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn ignores_non_property_nodes() {
        // A bare container tag and a style header must never be links, even one literally spelled
        // like a path-valued key would be (there is none here — these are structural nodes).
        let source = "MainWindow < UIWindow\n  Panel\n    id: body\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn empty_value_yields_no_link() {
        // `image-source:` with no value is not a link (nothing to open).
        let source = "Panel\n  image-source:\n";
        assert!(document_links(source).is_empty());
    }

    #[test]
    fn span_covers_exactly_the_path_and_leaves_the_key_out() {
        // The reported span underlines the path token only — never the `image-source:` key, the `:`,
        // or the separating space. A path with an interior `.` and `/` is reported verbatim, so the
        // extension and directory separators are inside the link.
        let source = "Panel\n  image-source: assets/ui/bg.9.png\n";
        let r = &document_links(source)[0];
        let text = &source[r.span.start..r.span.end];
        assert_eq!(text, "assets/ui/bg.9.png");
        assert_eq!(text, r.path, "span text and stored path agree");
        // The span sits strictly after the `: ` separator (the key/colon are not underlined).
        let colon = source.find(':').unwrap();
        assert!(
            r.span.start > colon + 1,
            "span starts past the `: ` separator"
        );
    }

    #[test]
    fn multiple_links_across_widgets_in_source_order() {
        let source =
            "A\n  image-source: a.png\nB\n  image-source: b.png\nC\n  image-source: c.png\n";
        let paths: Vec<String> = document_links(source).into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["a.png", "b.png", "c.png"]);
    }

    #[test]
    fn asset_ref_at_cursor_in_image_source_value_returns_it() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        let offset = at(source, "/images") + 1;
        let got = asset_ref_at(source, offset).expect("hit");
        assert_eq!(got.path, "/images/ui/window");
        assert_eq!(&source[got.span.start..got.span.end], "/images/ui/window");
    }

    #[test]
    fn asset_ref_at_cursor_on_key_is_none() {
        // The key position is `property_hover_at`'s job, not this locator's.
        let source = "Panel\n  image-source: /images/ui/window\n";
        assert!(asset_ref_at(source, at(source, "image-source") + 1).is_none());
    }

    #[test]
    fn asset_ref_at_cursor_in_non_path_property_value_is_none() {
        let source = "Panel\n  text: Hello World\n  color: red\n";
        assert!(asset_ref_at(source, at(source, "Hello") + 1).is_none());
        assert!(asset_ref_at(source, at(source, "red") + 1).is_none());
    }

    #[test]
    fn asset_ref_at_cursor_outside_any_property_is_none() {
        let source = "Panel\n  image-source: /images/ui/window\n";
        // On the widget tag name.
        assert!(asset_ref_at(source, at(source, "Panel")).is_none());
        assert!(asset_ref_at("", 0).is_none());
    }

    #[test]
    fn asset_ref_at_covers_each_path_property() {
        let source =
            "Button\n  icon: a.png\nOther\n  icon-source: b.png\nThird\n  image-source: c.png\n";
        assert_eq!(
            asset_ref_at(source, at(source, "a.png") + 1).map(|r| r.path),
            Some("a.png".to_owned())
        );
        assert_eq!(
            asset_ref_at(source, at(source, "b.png") + 1).map(|r| r.path),
            Some("b.png".to_owned())
        );
        assert_eq!(
            asset_ref_at(source, at(source, "c.png") + 1).map(|r| r.path),
            Some("c.png".to_owned())
        );
    }

    #[test]
    fn asset_ref_at_boundary_matches_half_open_convention() {
        // Half-open `[start, end)`: the first char of the path is a hit, and the last char is a hit,
        // but the offset exactly at `end` (one past the last char) is not — consistent with
        // `navigation::base_reference_at`'s `offset_just_past_base_is_not_a_hit`.
        let source = "Panel\n  image-source: a.png\n";
        let start = at(source, "a.png");
        let end = start + "a.png".len();
        assert!(asset_ref_at(source, start).is_some());
        assert!(asset_ref_at(source, end - 1).is_some());
        assert!(asset_ref_at(source, end).is_none());
    }

    #[test]
    fn asset_ref_at_agrees_with_document_links_no_regression_from_refactor() {
        // The shared `path_ref` helper still produces exactly what `document_links` produced before
        // the point-locator was factored out.
        let source = "A\n  image-source: a.png\nB\n  icon: b.png\nC\n  icon-source: c.png\n";
        let bulk = document_links(source);
        assert_eq!(bulk.len(), 3);
        for r in &bulk {
            let offset = r.span.start;
            assert_eq!(asset_ref_at(source, offset).as_ref(), Some(r));
        }
    }

    // --- asset path resolution ------------------------------------------------

    #[test]
    fn resolve_asset_candidates_maps_rooted_path_against_workspace_roots() {
        // A `/`-rooted OTClient "absolute" path is joined directly onto each root with the leading
        // `/` stripped (never against the doc dir — the bare root is always itself a mounted search
        // path; see `ASSET_MOUNT_DIRS`'s doc comment), **and** onto each root's `mods`/`modules`/
        // `data` subdirectory, in the engine's real overlay order.
        let doc_dir = Path::new("/project/modules/game_things");
        let roots = vec![PathBuf::from("/data-a"), PathBuf::from("/data-b")];
        let candidates = resolve_asset_candidates("/images/ui/window.png", doc_dir, &roots);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data-a/images/ui/window.png"),
                PathBuf::from("/data-a/mods/images/ui/window.png"),
                PathBuf::from("/data-a/modules/images/ui/window.png"),
                PathBuf::from("/data-a/data/images/ui/window.png"),
                PathBuf::from("/data-b/images/ui/window.png"),
                PathBuf::from("/data-b/mods/images/ui/window.png"),
                PathBuf::from("/data-b/modules/images/ui/window.png"),
                PathBuf::from("/data-b/data/images/ui/window.png"),
            ]
        );
    }

    #[test]
    fn resolve_asset_candidates_finds_an_asset_placed_directly_at_the_bare_install_root() {
        // The install root itself is always a mounted search path (`main.cpp`'s
        // `discoverWorkDir("init.lua")` mounts it via `PHYSFS_mount` and never unmounts it on
        // success, ahead of anything `init.lua` mounts later — see `ASSET_MOUNT_DIRS`'s doc comment
        // for the full trace). A CodeRabbit review of this crate (PR #51, Finding 2) claimed the bare
        // root was never a mount, reasoning only from `init.lua`'s three `addSearchPath` calls; this
        // pins the corrected, verified behavior instead. Real, shipped, autoloaded OTClient modules
        // rely on exactly this: `game_proficiency`/`game_healthcircle`/`game_cyclopedia` write
        // `/modules/…`/`/data/…`-prefixed `image-source` paths that resolve only via the bare root,
        // never the `mods`/`modules`/`data` overlay (which would need an impossible
        // `data/data/…` nesting).
        let roots = vec![PathBuf::from("/client-root")];
        let candidates =
            resolve_asset_candidates("/foo.png", Path::new("/irrelevant/doc/dir"), &roots);
        assert!(
            candidates.contains(&PathBuf::from("/client-root/foo.png")),
            "the bare install root must be a probed candidate: {candidates:?}"
        );
    }

    #[test]
    fn resolve_asset_candidates_finds_a_module_local_asset_under_the_modules_overlay() {
        // The fidelity finding this test locks in: a `/`-rooted path with no `data/` involved at all
        // is a real, common shape in the OTClient corpus (e.g. `/game_rewardwall/images/...`
        // resolves inside `modules/game_rewardwall/images/...`, the module's own asset folder) — see
        // `ASSET_MOUNT_DIRS`'s doc comment for the `init.lua`/`resourcemanager.cpp` trace. Treating
        // the workspace root as the single flat data root (the pre-fix behavior) would call this
        // missing.
        let base = std::env::temp_dir().join(format!(
            "otui-asset-overlay-{}-{}",
            std::process::id(),
            line!()
        ));
        let module_dir = base.join("modules").join("game_rewardwall").join("images");
        std::fs::create_dir_all(&module_dir).expect("mkdir");
        std::fs::write(module_dir.join("rewardButton.png"), b"png").expect("write asset");

        let candidates = resolve_asset_candidates(
            "/game_rewardwall/images/rewardButton",
            Path::new("/irrelevant/doc/dir"),
            std::slice::from_ref(&base),
        );
        assert!(
            candidates
                .iter()
                .any(|c| c == &module_dir.join("rewardButton.png")),
            "candidates should include the modules-overlay path: {candidates:?}"
        );
        assert!(candidates.iter().any(|c| c.is_file()));

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_asset_candidates_maps_relative_path_against_doc_dir() {
        // A relative path resolves against the current file's directory, ignoring workspace roots.
        let doc_dir = Path::new("/project/modules/game_things");
        let roots = vec![PathBuf::from("/data-root")];
        let candidates = resolve_asset_candidates("sprites/ok.png", doc_dir, &roots);
        assert_eq!(
            candidates,
            vec![PathBuf::from("/project/modules/game_things/sprites/ok.png")]
        );
    }

    #[test]
    fn resolve_asset_candidates_rooted_with_no_workspace_yields_nothing() {
        // Offline, a `/`-rooted path has no data root to resolve against when no workspace is open.
        let candidates = resolve_asset_candidates("/images/x.png", Path::new("/project/sub"), &[]);
        assert!(candidates.is_empty());
    }

    #[test]
    fn resolve_asset_candidates_appends_png_to_an_extensionless_path() {
        // OTUI authors omit the extension; the engine appends `.png`. The `.png` variant is probed
        // first, then the literal as a fallback — for every base the rooted path expands to (the
        // direct join, then each `mods`/`modules`/`data` overlay candidate).
        let roots = vec![PathBuf::from("/data")];
        let candidates =
            resolve_asset_candidates("/images/ui/button", Path::new("/project"), &roots);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data/images/ui/button.png"),
                PathBuf::from("/data/images/ui/button"),
                PathBuf::from("/data/mods/images/ui/button.png"),
                PathBuf::from("/data/mods/images/ui/button"),
                PathBuf::from("/data/modules/images/ui/button.png"),
                PathBuf::from("/data/modules/images/ui/button"),
                PathBuf::from("/data/data/images/ui/button.png"),
                PathBuf::from("/data/data/images/ui/button"),
            ]
        );
    }

    #[test]
    fn resolve_asset_candidates_keeps_an_explicit_extension_as_is() {
        // A path that already carries an extension is probed verbatim — no `.png.png`.
        let candidates = resolve_asset_candidates(
            "sprites/ok.png",
            Path::new("/project/mod"),
            &[PathBuf::from("/data")],
        );
        assert_eq!(
            candidates,
            vec![PathBuf::from("/project/mod/sprites/ok.png")]
        );
    }

    #[test]
    fn resolve_asset_candidates_appends_png_to_a_dotted_stem_that_is_not_already_png() {
        // Finding 4: `guessFilePath` (`resourcemanager.cpp`) concatenates `.png` onto the *whole*
        // literal filename unless it already ends in `.png` — it does not replace a final
        // extension. `icon.small` is not `.png`-terminated, so the engine loads `icon.small.png`;
        // the pre-fix `Path::extension().is_some()` check wrongly treated the `.small` as a real,
        // final extension and never probed the `.png` form at all.
        let roots = vec![PathBuf::from("/data")];
        let candidates =
            resolve_asset_candidates("/images/icon.small", Path::new("/project"), &roots);
        assert_eq!(
            candidates[0],
            PathBuf::from("/data/images/icon.small.png"),
            "the concatenated .png form must be probed first: {candidates:?}"
        );
        assert!(
            candidates.contains(&PathBuf::from("/data/images/icon.small")),
            "the literal must still be probed as a fallback: {candidates:?}"
        );
    }

    #[test]
    fn is_asset_sentinel_value_recognizes_an_inline_base64_image() {
        // Finding 1: `UIWidget::parseImageStyle` (`uiwidgetimage.cpp`) splits on `:`; when the first
        // segment is exactly "base64" and a second segment exists, the value is decoded straight
        // into a texture (`g_crypt.base64Decode` + `loadTexture`) — the filesystem is never touched,
        // so this can never be "missing".
        assert!(is_asset_sentinel_value("base64:iVBORw0KGgoAAAANSUhEUg=="));
        // Quoted, like the other sentinels this function already recognizes.
        assert!(is_asset_sentinel_value("\"base64:iVBORw0KGgo=\""));
        // `base64:` with nothing after the colon does not meet the engine's own `split.size() > 1`
        // guard — it falls through to ordinary (probably-missing) path resolution, not a sentinel.
        assert!(!is_asset_sentinel_value("base64:"));
        // The bare word "base64" (no colon at all) is just an unusual literal path, not the inline
        // scheme.
        assert!(!is_asset_sentinel_value("base64"));
        // Existing sentinels are unaffected.
        assert!(is_asset_sentinel_value(""));
        assert!(is_asset_sentinel_value("none"));
        assert!(!is_asset_sentinel_value("/images/real/path.png"));
    }

    #[test]
    fn is_runtime_variable_path_recognizes_a_dollar_substitution() {
        assert!(is_runtime_variable_path("$imagePath"));
        assert!(is_runtime_variable_path("/images/$name/icon.png"));
        assert!(!is_runtime_variable_path("/images/ui/window.png"));
    }
}
