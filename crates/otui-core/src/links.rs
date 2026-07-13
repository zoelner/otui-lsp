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
    /// The exact [`schema::PATH_PROPERTIES`] tag this value was found under (`"image-source"`,
    /// `"icon"`, or `"icon-source"`). Some resolution rules are property-specific — most notably
    /// [`is_asset_sentinel_value`], whose `""`/`none`/`base64:` short-circuits are only real for
    /// `image-source` (see that function's doc comment) — so callers need this alongside `path`.
    pub key: &'static str,
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
    if node.kind() == "property"
        && let Some(path_ref) = path_ref(node, source)
    {
        out.push(path_ref);
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
    // Look up (rather than just check membership of) the matching static tag so `PathRef::key` can
    // be a cheap `&'static str` instead of an owned copy of the source slice.
    let key = *schema::PATH_PROPERTIES.iter().find(|&&k| k == key_text)?;
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
        key,
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
/// **The install root itself is also always mounted** — a candidate a CodeRabbit review of this
/// crate (PR #51, Finding 2) claimed did not exist, reasoning only from `init.lua`'s three
/// `addSearchPath` calls above. That reasoning missed the earlier, unconditional C++ mount: `main()`
/// calls `g_resources.discoverWorkDir("init.lua")` (`src/main.cpp`) before any Lua ever runs;
/// `ResourceManager::discoverWorkDir` (`resourcemanager.cpp`) `PHYSFS_mount`s each install-root
/// candidate directory at the virtual root to test for `init.lua`, and on the directory that has it,
/// **breaks out of the loop without ever unmounting it** — so the install root stays mounted for the
/// rest of the session. Verified directly against the vendored `otclient` source, not just
/// `init.lua`: a real, autoloaded, shipped module corpus (`game_proficiency`, `game_healthcircle`,
/// `game_cyclopedia`, `game_inventory` — 35 distinct `image-source`/`icon` paths) writes
/// `/data/…`/`/modules/…`-prefixed asset paths that resolve **only** via this bare-root mount (never
/// via the `mods`/`modules`/`data` overlay, which would require an impossible doubly-nested
/// `data/data/…`) — concrete, on-disk proof the bare root is a real, load-bearing search path, not a
/// hypothetical one. [`resolve_asset_candidates`] must therefore keep probing it.
///
/// **But the bare root is the *lowest*-priority mount, not the highest** (PR #51 review round 2,
/// Finding B) — a correction to this doc comment's own earlier claim that it sat "ahead of
/// everything `init.lua` adds". `PHYSFS_mount(dir, mountPoint, appendToPath)`'s own header doc
/// (`physfs.h`) is explicit: `appendToPath` "nonzero to append to search path, zero to prepend", and
/// on overlap "the file earliest in the search path is selected". `discoverWorkDir` mounts the bare
/// root with `PHYSFS_mount(dir, nullptr, 0)` — prepend — *before* any of `init.lua` runs, so at that
/// instant it is alone at the front. But every `addSearchPath(path, true)` call above also prepends
/// (`resourcemanager.cpp`: `pushFront ? PHYSFS_mount(path, nullptr, 0) : …`), and each later prepend
/// pushes everything already mounted one step further back. Walking `init.lua`'s three calls in
/// order — `data` prepended (order: `[data, bare-root]`), then `modules` prepended (`[modules, data,
/// bare-root]`), then `mods` prepended (`[mods, modules, data, bare-root]`) — leaves the bare root
/// searched *last*, after all three named overlays, not first. This only changes which candidate a
/// first-match consumer (`document_link`'s target, the hover sprite preview) picks when the *same*
/// relative path exists under more than one mount; it does not change whether `missing-asset` fires,
/// since that only asks whether *any* candidate resolves. [`resolve_asset_candidates`] probes
/// `mods`/`modules`/`data` before the bare root to match.
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
/// * The raw value is first unwrapped with [`strip_matching_quotes`] — a quoted asset value like
///   `image-source: "images/ui/window"` is a real, if uncommon, OTML shape, and probing the
///   *literal, quote-included* text (`"images/ui/window"` as a file name) would never resolve. See
///   [`strip_matching_quotes`]'s doc comment for exactly which quoting this mirrors.
/// * A **`/`-rooted** path is an OTClient "absolute" path — relative to the mounted virtual root, not
///   the OS root or any single "data" directory. The leading `/` is stripped and the remainder is
///   joined onto each of `root/mods`, `root/modules`, `root/data` (the engine's real overlay, probed
///   in that order — first match wins), **then** onto the root directly (the bare-root mount — the
///   lowest-priority search path; see [`ASSET_MOUNT_DIRS`]'s doc comment for why it is probed last,
///   not first). With no `roots` at all a `/`-rooted path yields no candidates offline.
/// * Any **other** (relative) path is resolved against the current document's directory (approximating
///   the engine's "resolve relative to the currently executing script" rule).
/// * **Extensionless** (or non-`.png`-suffixed) paths get *only* a `.png`-appended variant — no
///   literal-as-fallback probe. See [`asset_probe_variants`]'s doc comment: the engine's
///   `guessFilePath` returns exactly one string, never tries a second.
///
/// Returns the candidates in probe order; the caller keeps the first that exists.
#[must_use]
pub fn resolve_asset_candidates(raw: &str, doc_dir: &Path, roots: &[PathBuf]) -> Vec<PathBuf> {
    let path = strip_matching_quotes(raw.trim());
    if path.is_empty() {
        return Vec::new();
    }
    let bases: Vec<PathBuf> = if let Some(rest) = path.strip_prefix('/') {
        // OTClient "absolute" = relative to the mounted virtual root; approximate the mount as each
        // of the root's conventional `mods`/`modules`/`data` subdirectories (the engine's real
        // overlay, highest priority first — see `ASSET_MOUNT_DIRS`'s doc comment), then the root
        // directly (the always-mounted, but lowest-priority, bare install root). Strip the leading
        // `/` so `join` does not discard the root.
        let mut bases = Vec::new();
        for root in roots {
            for mount in ASSET_MOUNT_DIRS {
                bases.push(root.join(mount).join(rest));
            }
            bases.push(root.join(rest));
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
/// a broken reference — but, per the engine source, **only for `image-source`** (`key` must be
/// exactly `"image-source"`; any other [`schema::PATH_PROPERTIES`] key — `icon`/`icon-source` —
/// always returns `false` here, see below). Verified against `UIWidget::parseImageStyle`
/// (`uiwidgetimage.cpp`):
/// ```cpp
/// if (value == "" || value == "none") { setImageSource("", base64); } // else resolve_path(value, ...)
/// ```
/// — `image-source: none` and `image-source: ""` are the documented ways to clear an inherited
/// image, found in the real corpus (`game_cyclopedia/tab/house/house.otui`,
/// `client_options/styles/controls/keybinds.otui`). The `value` being compared here is
/// `OTMLNode::value<std::string>()`'s *return*, not the raw stored text — that specialization
/// (`otml/otmlnode.h`) strips one layer of matching `"…"` **double**-quotes (and only double quotes;
/// a `'…'` value is returned unchanged, quotes and all) before any engine code ever compares or
/// resolves it. `otmlparser.cpp`'s own `stripQuotes`/`normalizeValue` — which does accept `'…'` too —
/// is a *different*, unrelated mechanism: it only runs on `&alias` declarations and `$variable`
/// substitution at parse time, never on an ordinary `key: value` property read. This mirrors the one
/// that actually applies to `image-source`/`icon`/`icon-source` reads: see
/// [`strip_matching_quotes`].
///
/// **`icon`/`icon-source` never short-circuit at all** — a correction to this function's own
/// previous doc comment, which applied the same `""`/`none`/`base64:` sentinels to every
/// `PATH_PROPERTIES` key as a "deliberately conservative generalization" (PR #51 review round 2,
/// Finding C). `uiwidgetbasestyle.cpp`'s icon dispatch is unconditional for both tags:
/// ```cpp
/// else if (node->tag() == "icon")        setIcon(stdext::resolve_path(node->value(), node->source()));
/// else if (node->tag() == "icon-source") setIcon(stdext::resolve_path(node->value(), node->source()));
/// ```
/// — no `value == "" || value == "none"` guard exists here the way it does for `image-source`, and
/// there is no `base64:`-scheme split either. Even `icon: ""` is not harmlessly skipped:
/// `stdext::resolve_path` (`stdext/string.cpp`) only returns its `filePath` argument unchanged when
/// it starts with `/`; an empty (or otherwise relative) `filePath` instead falls through to
/// `sourcePath.substr(0, slashPos + 1) + filePath` — the *containing directory* of the `.otui` file
/// itself, a non-empty string — which `setIcon` (`uiwidgetbasestyle.cpp`) then unconditionally hands
/// to `g_textures.getTexture`. So an `icon: ""`/`icon: none`/`icon: base64:...` value is not a
/// verified no-op the way `image-source`'s is; it is a genuine (and, for `none`/`base64:`, almost
/// certainly failing) resolution attempt in the real engine. Treating those as sentinels for icon
/// would be a false negative (an icon-referencing typo the diagnostic silently misses) — the wrong
/// direction for a linter meant to catch broken references, and the reverse of applying them too
/// narrowly.
///
/// Also recognizes an inline `base64:<blob>` value as "not a path" for `image-source`:
/// `UIWidget::parseImageStyle` (`uiwidgetimage.cpp`) splits `image-source` on `:` and, when the first
/// segment is exactly `"base64"` and a second segment exists, decodes the rest with
/// `g_crypt.base64Decode()` and loads the texture straight from the decoded bytes
/// (`setImageSource(.., base64=true)` → `g_textures.loadTexture(stream)`) — **the filesystem is
/// never touched**, so there is no path to probe and nothing that can ever be "missing" on disk.
/// `base64:` alone (no second segment) is not this case — the engine's own `split.size() > 1` guard
/// requires content after the colon — and falls through to ordinary path resolution, matching the
/// engine's `value = split[0]` behavior for that shape.
#[must_use]
pub fn is_asset_sentinel_value(key: &str, raw: &str) -> bool {
    if key != "image-source" {
        return false;
    }
    let unquoted = strip_matching_quotes(raw.trim());
    if unquoted.is_empty() || unquoted == "none" {
        return true;
    }
    if let Some((scheme, rest)) = unquoted.split_once(':')
        && scheme == "base64"
        && !rest.is_empty()
    {
        return true;
    }
    false
}

/// Strip one layer of matching `"…"` **double**-quotes from `s`, mirroring `OTMLNode::value<std::
/// string>()` (`otml/otmlnode.h`) — the function the engine's widget style parsers actually call to
/// read a node's string value (`node->value()` in `parseImageStyle`/`parseBaseStyle`), which is what
/// determines the literal text `image-source`/`icon`/`icon-source` resolution ever sees. `s`
/// unchanged if it is not wrapped in double quotes — in particular a `'…'`-quoted value is returned
/// **as-is, quotes included**: unlike `otmlparser.cpp`'s separate, alias-only `stripQuotes` (which
/// does accept `'…'`), the value-read specialization checks only `"` at both ends. A file that is
/// genuinely named with literal single-quote characters is a vanishingly unlikely real path, but
/// silently also stripping `'…'` here (the pre-fix behavior) would treat `icon: 'x.png'` as
/// resolving to `x.png` when the real engine would search for the six-character filename `'x.png'`
/// and almost certainly fail — a false negative this fix removes.
///
/// Deliberately does **not** replicate `value<std::string>()`'s further backslash-escape unescaping
/// (`\\`→`\`, `\"`→`"`, `\t`→tab, `\n`→newline, `\'`→`'`, applied as a *sequence* of whole-string
/// substring replacements, not a single left-to-right decode pass — so it is itself order-dependent
/// and can produce surprising results for adjacent escapes). No quoted value, let alone an escaped
/// one, appears anywhere in the real `.otui` corpus this crate is validated against, and a real
/// filesystem path essentially never contains a backslash; reproducing that quirk exactly would add
/// meaningfully more surface to a helper that feeds straight into filesystem probing, for a shape
/// that has never been observed.
fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        return &s[1..s.len() - 1];
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
/// — **not** "replace the extension", and **not** "try both and keep whichever exists" — it
/// `return`s exactly one string, and every caller (`TextureManager::getTexture`,
/// `texturemanager.cpp`) hands that single result straight to the loader with no fallback attempt if
/// it fails. A path is probed as-is only when its filename already ends in `.png` (case-sensitive,
/// matching `ends_with`); every other path — extensionless (`.../button`) or carrying some *other*
/// extension (`.../icon.small`, `.../icon.9`) — gets `.png` concatenated onto the full literal name
/// (`button.png`, `icon.small.png`, `icon.9.png`) and **only** that form is ever tried: the real
/// engine never falls back to loading `.../button`, `.../icon.small`, or `.../icon.9` literally (PR
/// #51 review round 2, Finding D — the pre-fix behavior additionally probed the literal path as a
/// "harmless fallback", which is a false negative: it can mark a path resolved when the real engine,
/// probing only the concatenated `.png` form, would fail to load it). Keying off
/// `Path::extension().is_some()` (instead of this exact `ends_with(".png")` check) would separately
/// wrongly treat any dotted stem as "already has its final extension" and skip the `.png` probe
/// entirely — a false positive for the common `name.<variant>` sprite-sheet naming shape.
fn asset_probe_variants(base: PathBuf) -> Vec<PathBuf> {
    let ends_with_png = base
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".png"));
    if ends_with_png {
        vec![base]
    } else {
        let mut with_png = base.into_os_string();
        with_png.push(".png");
        vec![PathBuf::from(with_png)]
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
    fn path_ref_key_is_the_exact_property_tag() {
        // `PathRef::key` must carry the exact matched tag, not just any truthy marker — callers
        // (`is_asset_sentinel_value`) key their engine-verified special cases off it.
        let source = "A\n  image-source: a.png\nB\n  icon: b.png\nC\n  icon-source: c.png\n";
        let links = document_links(source);
        let keys: Vec<&str> = links.iter().map(|r| r.key).collect();
        assert_eq!(keys, ["image-source", "icon", "icon-source"]);
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
        // A `/`-rooted OTClient "absolute" path is joined onto each root's `mods`/`modules`/`data`
        // subdirectory (the engine's real overlay, in its actual first-match-wins priority order),
        // **then** onto the root directly (the bare-root mount, which is always present but is the
        // *lowest*-priority search path — see `ASSET_MOUNT_DIRS`'s doc comment for the
        // `PHYSFS_mount`/`init.lua` prepend trace that establishes this order).
        let doc_dir = Path::new("/project/modules/game_things");
        let roots = vec![PathBuf::from("/data-a"), PathBuf::from("/data-b")];
        let candidates = resolve_asset_candidates("/images/ui/window.png", doc_dir, &roots);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data-a/mods/images/ui/window.png"),
                PathBuf::from("/data-a/modules/images/ui/window.png"),
                PathBuf::from("/data-a/data/images/ui/window.png"),
                PathBuf::from("/data-a/images/ui/window.png"),
                PathBuf::from("/data-b/mods/images/ui/window.png"),
                PathBuf::from("/data-b/modules/images/ui/window.png"),
                PathBuf::from("/data-b/data/images/ui/window.png"),
                PathBuf::from("/data-b/images/ui/window.png"),
            ]
        );
    }

    #[test]
    fn resolve_asset_candidates_finds_an_asset_placed_directly_at_the_bare_install_root() {
        // The install root itself is always a mounted search path (`main.cpp`'s
        // `discoverWorkDir("init.lua")` mounts it via `PHYSFS_mount` and never unmounts it on
        // success — see `ASSET_MOUNT_DIRS`'s doc comment for the full trace). A CodeRabbit review of
        // this crate (PR #51, Finding 2) claimed the bare root was never a mount, reasoning only from
        // `init.lua`'s three `addSearchPath` calls; this pins the corrected, verified behavior
        // instead. Real, shipped, autoloaded OTClient modules rely on exactly this:
        // `game_proficiency`/`game_healthcircle`/`game_cyclopedia` write `/modules/…`/`/data/…`
        // -prefixed `image-source` paths that resolve only via the bare root, never the
        // `mods`/`modules`/`data` overlay (which would need an impossible `data/data/…` nesting) —
        // it is still probed even though (per Finding B) it is the lowest-priority one.
        let roots = vec![PathBuf::from("/client-root")];
        let candidates =
            resolve_asset_candidates("/foo.png", Path::new("/irrelevant/doc/dir"), &roots);
        assert!(
            candidates.contains(&PathBuf::from("/client-root/foo.png")),
            "the bare install root must be a probed candidate: {candidates:?}"
        );
    }

    #[test]
    fn resolve_asset_candidates_probes_the_overlay_before_the_bare_root() {
        // Finding B: `mods`/`modules`/`data` are searched (first-match-wins) *ahead* of the bare
        // install root, per the `PHYSFS_mount` prepend trace in `ASSET_MOUNT_DIRS`'s doc comment —
        // not the other way around. This only matters for a first-match consumer
        // (`document_link`/the hover preview); `missing-asset` does not care which candidate wins,
        // only whether any does.
        let roots = vec![PathBuf::from("/client-root")];
        let candidates =
            resolve_asset_candidates("/foo.png", Path::new("/irrelevant/doc/dir"), &roots);
        let overlay = candidates
            .iter()
            .position(|c| c == &PathBuf::from("/client-root/mods/foo.png"))
            .expect("mods overlay candidate present");
        let bare = candidates
            .iter()
            .position(|c| c == &PathBuf::from("/client-root/foo.png"))
            .expect("bare-root candidate present");
        assert!(
            overlay < bare,
            "the mods overlay must be probed before the bare root: {candidates:?}"
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
        // OTUI authors omit the extension; the engine appends `.png` and probes *only* that form —
        // see `asset_probe_variants`'s doc comment (Finding D): `guessFilePath` returns exactly one
        // string, never a literal-path fallback — for every base the rooted path expands to (each
        // `mods`/`modules`/`data` overlay candidate, then the bare root).
        let roots = vec![PathBuf::from("/data")];
        let candidates =
            resolve_asset_candidates("/images/ui/button", Path::new("/project"), &roots);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/data/mods/images/ui/button.png"),
                PathBuf::from("/data/modules/images/ui/button.png"),
                PathBuf::from("/data/data/images/ui/button.png"),
                PathBuf::from("/data/images/ui/button.png"),
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
        // `guessFilePath` (`resourcemanager.cpp`) concatenates `.png` onto the *whole* literal
        // filename unless it already ends in `.png` — it does not replace a final extension.
        // `icon.small` is not `.png`-terminated, so the engine loads `icon.small.png` and *only*
        // `icon.small.png` (Finding D — no literal-path fallback; see `asset_probe_variants`'s doc
        // comment). Keying off `Path::extension().is_some()` instead would wrongly treat the
        // `.small` as a real, final extension and never probe the `.png` form at all.
        let roots = vec![PathBuf::from("/data")];
        let candidates =
            resolve_asset_candidates("/images/icon.small", Path::new("/project"), &roots);
        assert_eq!(
            candidates[0],
            PathBuf::from("/data/mods/images/icon.small.png"),
            "the concatenated .png form must be probed: {candidates:?}"
        );
        assert!(
            !candidates.contains(&PathBuf::from("/data/mods/images/icon.small")),
            "the literal must NOT be probed — the real engine never falls back to it: {candidates:?}"
        );
    }

    #[test]
    fn resolve_asset_candidates_strips_a_double_quoted_value_before_resolving() {
        // Finding A: `image-source: "images/ui/window"` is quoted OTML; the engine's
        // `OTMLNode::value<std::string>()` (`otml/otmlnode.h`) strips the wrapping `"…"` before any
        // widget style parser ever sees the value, so the probed candidate must be the *unquoted*
        // path, not the literal quote-included text (which could never exist on disk).
        let roots = vec![PathBuf::from("/data")];
        let candidates =
            resolve_asset_candidates("\"sprites/ok.png\"", Path::new("/project"), &roots);
        assert_eq!(candidates, vec![PathBuf::from("/project/sprites/ok.png")]);
    }

    #[test]
    fn resolve_asset_candidates_does_not_strip_single_quotes() {
        // The engine's value-read quote-stripping (`OTMLNode::value<std::string>()`) checks only a
        // `"…"` wrapper; a `'…'` value is returned unchanged, quotes included, and the real engine
        // would search for that literal filename — `'ok.png'` does not itself end in `.png` (it ends
        // in a closing quote), so the engine's `guessFilePath` would append `.png` onto the whole
        // quoted literal, exactly like any other non-`.png`-terminated name. Stripping single quotes
        // here (the pre-fix `strip_matching_quotes`, shared with the alias-only
        // `otmlparser.cpp::stripQuotes`) would silently produce a different, wrong candidate.
        let candidates =
            resolve_asset_candidates("'ok.png'", Path::new("/project"), &[PathBuf::from("/data")]);
        assert_eq!(candidates, vec![PathBuf::from("/project/'ok.png'.png")]);
    }

    #[test]
    fn is_asset_sentinel_value_recognizes_an_inline_base64_image() {
        // `UIWidget::parseImageStyle` (`uiwidgetimage.cpp`) splits `image-source` on `:`; when the
        // first segment is exactly "base64" and a second segment exists, the value is decoded
        // straight into a texture (`g_crypt.base64Decode` + `loadTexture`) — the filesystem is never
        // touched, so this can never be "missing".
        assert!(is_asset_sentinel_value(
            "image-source",
            "base64:iVBORw0KGgoAAAANSUhEUg=="
        ));
        // Quoted, like the other sentinels this function already recognizes.
        assert!(is_asset_sentinel_value(
            "image-source",
            "\"base64:iVBORw0KGgo=\""
        ));
        // `base64:` with nothing after the colon does not meet the engine's own `split.size() > 1`
        // guard — it falls through to ordinary (probably-missing) path resolution, not a sentinel.
        assert!(!is_asset_sentinel_value("image-source", "base64:"));
        // The bare word "base64" (no colon at all) is just an unusual literal path, not the inline
        // scheme.
        assert!(!is_asset_sentinel_value("image-source", "base64"));
        // Existing sentinels are unaffected.
        assert!(is_asset_sentinel_value("image-source", ""));
        assert!(is_asset_sentinel_value("image-source", "none"));
        assert!(!is_asset_sentinel_value(
            "image-source",
            "/images/real/path.png"
        ));
    }

    #[test]
    fn is_asset_sentinel_value_never_applies_to_icon_or_icon_source() {
        // Finding C: `uiwidgetbasestyle.cpp`'s `icon`/`icon-source` dispatch is unconditional
        // (`setIcon(stdext::resolve_path(node->value(), node->source()))`) — no `""`/`none` guard the
        // way `image-source` has, and no `base64:`-scheme split either. Applying `image-source`'s
        // sentinels to icon values would be a false negative: a genuinely broken `icon: none` typo
        // would go unflagged.
        for value in ["", "none", "base64:iVBORw0KGgo=", "\"\""] {
            assert!(
                !is_asset_sentinel_value("icon", value),
                "icon: {value:?} must not be treated as a sentinel"
            );
            assert!(
                !is_asset_sentinel_value("icon-source", value),
                "icon-source: {value:?} must not be treated as a sentinel"
            );
        }
        // A genuinely real icon path is unaffected either way.
        assert!(!is_asset_sentinel_value("icon", "/images/icons/ok.png"));
    }

    #[test]
    fn is_runtime_variable_path_recognizes_a_dollar_substitution() {
        assert!(is_runtime_variable_path("$imagePath"));
        assert!(is_runtime_variable_path("/images/$name/icon.png"));
        assert!(!is_runtime_variable_path("/images/ui/window.png"));
    }
}
