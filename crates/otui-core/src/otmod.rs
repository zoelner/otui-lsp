//! Reading a module manifest's `scripts:` list — the other half (alongside
//! [`lua_ui_loads`](crate::lua_ui_loads)) of the module-association mechanism `otui-lsp-server` uses
//! to pair a controller with its UI beyond [`lua_refs`](crate::lua_refs)'s same-stem fast path.
//!
//! A `.otmod` file is OTClient's module manifest — **the same OTML grammar** every `.otui` file
//! parses with (`Module::parse`, `src/framework/core/module.cpp`, reads it through the identical
//! `OTMLDocument`/`OTMLNode` machinery an `.otui` style sheet does). A minimal example
//! (`modules/client_topmenu/topmenu.otmod`):
//!
//! ```otml
//! Module
//!   name: client_topmenu
//!   scripts: [ topmenu ]
//! ```
//!
//! `scripts:` names the module's Lua controller file(s), **`.lua` implied**, relative to the
//! module's own directory (`stdext::resolve_path(tmp->value(), node->source())` in the engine). Two
//! shapes are both legal OTML and both appear in the real engine corpus:
//!
//! ```otml
//!   scripts: [ ctrl1, ctrl2 ]     -- inline array (the overwhelmingly common form)
//! ```
//! ```otml
//!   scripts:
//!     - ctrl1                    -- indented list-item block (rarer; e.g. game_quickloot)
//! ```
//!
//! A single entry may itself be a subdirectory path (`classes/geometry`, `const/inspect_const`) or
//! already carry an explicit `.lua` extension (`game_rewardwall.lua`) — the engine strips *whatever*
//! extension is present via `std::filesystem::path::replace_extension()` (no argument — it does not
//! check for `.lua` specifically, so this is case-insensitive and not `.lua`-only) before later
//! unconditionally appending `.lua` back (`LuaInterface::loadScript` → `guessFilePath`), so the two
//! spellings are equivalent; [`otmod_scripts`] mirrors that exact stripping (see
//! [`strip_extension`](self::strip_extension) for the detail) for the same reason.
//!
//! A trailing `*` (alone, or ending a subdirectory path — `scripts: [lib, effects, *]`, `scripts: [
//! game_cyclopedia, tab/*, utils]`) is a directory wildcard: "every `.lua` file under this
//! subdirectory, recursively" (`module.cpp`: `path.ends_with('*')` → `g_resources.listDirectoryFiles`
//! with the recursive flag). This module hands the raw `*`-suffixed entry back unresolved — it does
//! no I/O and cannot list a directory — leaving the recursive listing to the server, which has real
//! filesystem access.
//!
//! ## Parser choice: the real grammar, not a text scan
//!
//! Unlike [`lua_refs`](crate::lua_refs)/[`lua_ui_loads`](crate::lua_ui_loads) (no Lua grammar exists
//! in this workspace, so those scan byte-oriented), a `.otmod` file **is** OTML, and this crate
//! already owns a real, tested OTML grammar ([`crate::syntax::SyntaxTree`]/tree-sitter-otui). Parsing
//! with it — rather than hand-rolling a second, approximate `scripts:` line-scanner — gets comment
//! handling, quoted-vs-bare array items, and the two `scripts:` shapes above all correct for free,
//! from the same grammar every other feature in this crate already trusts for `.otui`. A text scan
//! would have to re-derive all of that (e.g. distinguishing a real `scripts: [...]` from one written
//! inside a `//` comment) and would drift from the grammar exactly the way
//! [`crate::lua_ui_loads`]'s doc comment warns a second Lua-as-text scanner would.
use crate::syntax::SyntaxTree;
use tree_sitter::Node;

/// The property key this module looks for, top-level or nested (spec: OTClient's own
/// `Module::parse` reads `moduleNode->get("scripts")`, which searches the whole node regardless of
/// depth — mirrored here by descending through every named child, not just `Module`'s direct ones).
const SCRIPTS_KEY: &str = "scripts";

/// The module's Lua controller entries named by its `.otmod` `scripts:` property, in document order,
/// exactly as written (a trailing `.lua` extension stripped — see the module doc comment — but a
/// subdirectory path or a trailing `*` wildcard left intact for the caller to resolve). Returns an
/// empty `Vec` when `source` fails to parse, has no `scripts:` property, that property's value is
/// empty (`scripts: []`), or the parsed tree contains **any** `ERROR`/`MISSING` node anywhere —
/// every case is "this module declares no controllers", never an error. The last of those is a hard
/// safety gate, not a convenience: tree-sitter's error recovery can leave a perfectly well-formed
/// `scripts: [ ... ]` `property` node sitting right next to unrelated broken syntax elsewhere in the
/// same file (e.g. a stray tab before a `:` a few lines up), so scanning the tree without checking
/// `has_error` first would happily recover and pair a controller out of a manifest the engine's own
/// `OTMLDocument` parser would never accept — exactly the false-pairing outcome this whole mechanism
/// exists to avoid. Mirrors [`crate::format::format`]'s identical gate.
#[must_use]
pub fn otmod_scripts(source: &str) -> Vec<String> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    if tree.has_error() {
        return Vec::new();
    }
    let mut out = Vec::new();
    find_scripts_property(tree.root(), source, &mut out);
    out
}

/// Depth-first search collecting every `property` node whose key is `scripts` into `out` — not just
/// the first one found. A matched node stops its *own* subtree from being searched further (a
/// `property` node never nests another `property` in this grammar, so there is nothing left to find
/// under it), but the search keeps walking every sibling and ancestor-sibling subtree afterwards, so
/// a second, third, etc. `scripts:` elsewhere in the same manifest is collected too. There is no
/// reason to expect more than one `scripts:` in a well-formed manifest, but nothing here assumes
/// otherwise — the grammar tree is small and walking all of it costs nothing measurable.
fn find_scripts_property(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    if node.kind() == "property"
        && let Some(key) = node.child_by_field_name("key")
        && node_text(key, source) == SCRIPTS_KEY
    {
        collect_scripts_value(node, source, out);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        find_scripts_property(child, source, out);
    }
}

/// Collect every script entry from a `scripts:` `property` node, whichever of the two shapes it was
/// written in:
///
/// * an inline value (`scripts: [ a, b ]`, or even a single bare `scripts: a`) — read from the
///   property's `value` field;
/// * an indented `- item` block (`scripts:\n  - a`) — the grammar inlines a hidden `_block`'s
///   statements as direct named children of the `property` node itself (mirrors how
///   [`crate::ids`]'s `collect_local_ids` walks a container's own block), so this reads every
///   `list_item` child instead.
fn collect_scripts_value(property: Node<'_>, source: &str, out: &mut Vec<String>) {
    if let Some(value) = property.child_by_field_name("value") {
        collect_scalar_or_array(value, source, out);
        return;
    }
    let mut cursor = property.walk();
    for child in property.named_children(&mut cursor) {
        if child.kind() == "list_item"
            && let Some(value) = child.child_by_field_name("value")
            && let Some(name) = normalize_entry(value, source)
        {
            out.push(name);
        }
    }
}

/// Collect scalar entries out of a property's value node: every named child of an `inline_array`, or
/// the value itself when it is not an array at all (a bare `scripts: single` with no brackets —
/// not observed in the real corpus, but legal OTML, and rejecting it would silently drop a
/// module's only controller).
fn collect_scalar_or_array(value: Node<'_>, source: &str, out: &mut Vec<String>) {
    if value.kind() == "inline_array" {
        let mut cursor = value.walk();
        for item in value.named_children(&mut cursor) {
            if let Some(name) = normalize_entry(item, source) {
                out.push(name);
            }
        }
    } else if let Some(name) = normalize_entry(value, source) {
        out.push(name);
    }
}

/// The script name text for a single array-item/scalar node, with any extension stripped (see
/// [`strip_extension`]) — or `None` for a node kind that is never a sensible script name (`color`,
/// `number`, `boolean`, `variable`, `null`), which contributes nothing rather than a garbage entry.
fn normalize_entry(node: Node<'_>, source: &str) -> Option<String> {
    let raw = match node.kind() {
        "identifier" | "plain_value" => node_text(node, source).to_owned(),
        "string" => strip_quotes(node_text(node, source)),
        _ => return None,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(strip_extension(trimmed))
}

/// Strip whatever extension (if any) the entry's final path component carries — mirroring
/// `Module::parse`'s `std::filesystem::path(...).replace_extension()` call with **no** argument
/// (`module.cpp`), which unconditionally discards the current extension rather than checking for
/// `.lua` specifically. That matters for two reasons the engine's own code makes plain:
///
/// * it is case-insensitive by construction — `Topmenu.LUA`/`topmenu.Lua`/`topmenu.lua` all lose
///   their extension the same way, since nothing here compares the stripped text against the
///   literal string `"lua"`;
/// * it is not specific to `.lua` at all — a hypothetical `foo.bar` entry would just as surely lose
///   its `.bar`, because the C++ side calls the extensionless overload, not one that only strips a
///   recognized extension.
///
/// The stripped result is never re-suffixed here (the caller/server appends `.lua` when resolving a
/// path on disk) because `LuaInterface::loadScript`'s own `guessFilePath(filePath, "lua")` — which
/// performs that append — only *skips* appending when `filePath` already ends, case-sensitively,
/// in literal `.lua`; since `replace_extension()` has already stripped the module's extension by
/// the time `loadScript` runs, that skip-branch is unreachable for a `scripts:` entry. `.lua` is
/// unconditionally appended, on top of whatever `replace_extension()` left behind.
///
/// Left untouched: an entry with no extension at all (`topmenu`, `classes/geometry`, `game_forge`),
/// and a trailing `*` wildcard (`tab/*`, alone or ending a subdirectory) — neither has a dot in its
/// final path component, so [`std::path::Path::extension`] reports `None` for both and this is a
/// no-op.
fn strip_extension(entry: &str) -> String {
    let path = std::path::Path::new(entry);
    if path.extension().is_some() {
        path.with_extension("").to_string_lossy().into_owned()
    } else {
        entry.to_owned()
    }
}

/// The exact source text a node spans.
fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    let span = SyntaxTree::span_of(node);
    &source[span.start..span.end]
}

/// Strip a single leading/trailing matching quote (`'` or `"`) from a `string` node's raw text — the
/// grammar's `string` token always includes its delimiters, unlike `identifier`/`plain_value`.
///
/// **Deliberate limitation:** this does not decode OTML string escape sequences (`\'`, `\"`, `\\`,
/// `\n`, ...) inside the body — it only removes the delimiters. The real engine corpus's `scripts:`
/// values are, without a single exception, bare identifiers or simple subdirectory paths (grep
/// confirms zero quoted entries anywhere in `modules/**/*.otmod`, let alone an escaped one), so a
/// full escape decoder here would handle a case that has never once occurred in practice. Should a
/// genuinely escaped script name ever appear, this returns the escape sequence's literal text
/// (`\'` stays `\'`) rather than a decoded quote — never a silently wrong path, just an
/// unresolved/mismatched one that fails to pair rather than mis-pairing.
fn strip_quotes(raw: &str) -> String {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' || first == b'"') && first == last {
            return raw[1..raw.len() - 1].to_owned();
        }
    }
    raw.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_bracketed_entry_is_read() {
        let src = "Module\n  name: client_topmenu\n  scripts: [ topmenu ]\n";
        assert_eq!(otmod_scripts(src), ["topmenu"]);
    }

    #[test]
    fn multiple_entries_preserve_document_order() {
        let src = "Module\n  scripts: [ wheel, classes/geometry, classes/buttons ]\n";
        assert_eq!(
            otmod_scripts(src),
            ["wheel", "classes/geometry", "classes/buttons"]
        );
    }

    #[test]
    fn an_explicit_lua_extension_is_stripped() {
        let src = "Module\n  scripts: [ game_rewardwall.lua ]\n";
        assert_eq!(otmod_scripts(src), ["game_rewardwall"]);
    }

    #[test]
    fn a_differently_cased_lua_extension_is_stripped_too() {
        // The engine's `replace_extension()` (no argument) does not compare against the literal
        // string "lua" at all — it strips whatever extension is present, case be damned. `.LUA`,
        // `.Lua`, and mixed case must all behave exactly like lowercase `.lua`.
        let src = "Module\n  scripts: [ Topmenu.LUA, other.Lua ]\n";
        assert_eq!(otmod_scripts(src), ["Topmenu", "other"]);
    }

    #[test]
    fn a_non_lua_extension_is_stripped_the_same_way() {
        // Not observed in the real corpus, but the engine's `replace_extension()` has no special
        // case for `.lua` — any dotted suffix on the entry's final path component is discarded the
        // same way, because `LuaInterface::loadScript` unconditionally re-appends `.lua` afterwards
        // regardless of what (if anything) `replace_extension()` left behind.
        let src = "Module\n  scripts: [ foo.bar ]\n";
        assert_eq!(otmod_scripts(src), ["foo"]);
    }

    #[test]
    fn a_trailing_wildcard_entry_is_kept_unresolved() {
        // Real corpus shapes: `game_attachedeffects/attachedeffects.otmod`'s `[lib, effects, *]` and
        // `game_cyclopedia/game_cyclopedia.otmod`'s `[ game_cyclopedia, tab/*, utils]`. This module
        // does no I/O, so a wildcard is handed back to the caller untouched, not resolved here.
        let src = "Module\n  scripts: [lib, effects, *]\n";
        assert_eq!(otmod_scripts(src), ["lib", "effects", "*"]);

        let src2 = "Module\n  scripts: [ game_cyclopedia, tab/*, utils]\n";
        assert_eq!(otmod_scripts(src2), ["game_cyclopedia", "tab/*", "utils"]);
    }

    #[test]
    fn an_indented_list_item_block_is_read() {
        // `modules/game_quickloot/quickloot.otmod`'s real shape.
        let src = "Module\n  scripts:\n    - quickloot\n  sandboxed: true\n";
        assert_eq!(otmod_scripts(src), ["quickloot"]);
    }

    #[test]
    fn a_quoted_entry_has_its_quotes_stripped() {
        let src = "Module\n  scripts: [ 'quoted', \"double\" ]\n";
        assert_eq!(otmod_scripts(src), ["quoted", "double"]);
    }

    #[test]
    fn a_missing_scripts_property_yields_an_empty_list() {
        let src = "Module\n  name: client_assets\n  sandboxed: true\n";
        assert!(otmod_scripts(src).is_empty());
    }

    #[test]
    fn an_empty_array_yields_an_empty_list() {
        let src = "Module\n  scripts: []\n";
        assert!(otmod_scripts(src).is_empty());
    }

    #[test]
    fn a_scripts_line_inside_a_comment_is_not_read() {
        let src = "Module\n  // scripts: [ ghost ]\n  name: x\n";
        assert!(otmod_scripts(src).is_empty());
    }

    #[test]
    fn a_malformed_document_with_a_recoverable_scripts_property_yields_no_scripts() {
        // A stray tab before a property's `:` produces an ERROR node right next to an otherwise
        // perfectly well-formed `scripts: [ ghost ]` sibling — tree-sitter's error recovery still
        // hands back a clean `property` node for `scripts`, so scanning the tree without first
        // checking `has_error()` would recover and return `["ghost"]` even though the manifest
        // overall does not parse cleanly. That is exactly the false controller<->UI pairing this
        // gate exists to prevent, so the whole document yields no scripts once any part of it fails
        // to parse.
        let src = "Module\n  name\tbroken: value\n  scripts: [ ghost ]\n";
        assert!(otmod_scripts(src).is_empty());
    }

    #[test]
    fn unparseable_source_yields_an_empty_list_rather_than_panicking() {
        // `SyntaxTree::parse` is error-tolerant (tree-sitter never fails outright on valid UTF-8), so
        // this exercises the defensive `let Some(tree) = ... else` branch stays reachable and safe
        // even though it is not expected to trigger against real input.
        assert!(otmod_scripts("").is_empty());
    }

    #[test]
    fn a_multi_word_scripts_list_matches_a_real_engine_manifest() {
        // `modules/game_analyser/analyser.otmod`, verbatim.
        let src = "Module\n  scripts: [ analyser, classes/Controller, classes/HuntingAnalyser, \
                   classes/LootAnalyser, classes/SupplyAnalyser, classes/ImpactAnalyser, \
                   classes/InputAnalyser, classes/XPAanalyser, classes/DropTrackerAnalyser, \
                   classes/PartyHuntAnalyser, classes/BossCooldown ]\n";
        let scripts = otmod_scripts(src);
        assert_eq!(scripts.len(), 11);
        assert_eq!(scripts[0], "analyser");
        assert_eq!(scripts[1], "classes/Controller");
        assert_eq!(scripts.last().unwrap(), "classes/BossCooldown");
    }
}
