//! Style-inheritance graph primitives (spec §5.2/§5.3): the pure core behind
//! `textDocument/typeDefinition` and `textDocument/implementation`, and the substrate a later
//! `textDocument/typeHierarchy` node will reuse.
//!
//! OTML expresses inheritance as `Name < Base` (a top-level [`style_header`]). This module maps that
//! `Name < Base` graph onto the two navigation directions:
//!
//! * **Up the graph — "what type is this?"** [`style_type_at`] answers *which style name the symbol
//!   under the cursor resolves to* — the tag of a **widget instance** (a `container`, at any depth),
//!   or the declared-name / base token of a **top-level** `style_header`. The server then resolves
//!   that name to its declaration(s) via [`style_declarations`] (the typeDefinition target).
//! * **Down the graph — "who derives from this?"** [`direct_subtypes`] lists the styles in one
//!   document whose base equals a given name (the implementation target).
//!
//! ## Fidelity notes (mirroring [`references`](crate::references) / [`style_index`](crate::style_index))
//!
//! * **Exact, case-sensitive name match.** Inheritance is keyed by exact string equality (the engine's
//!   `UIManager::m_styles`), so `Panel` and `panel` are different types. [`direct_subtypes`] compares
//!   bases with `==`, exactly like [`extract_style_defs`](crate::style_index::extract_style_defs).
//! * **Only top-level `Name < Base` headers are style declarations / subtypes.** A `Name < Base`
//!   nested in a widget block is an instance, not an inheritance declaration; only the document root's
//!   direct children are scanned as `style_header`s (matching `extract_style_defs` and
//!   [`style_name_occurrences`](crate::references::style_name_occurrences)).
//! * **Widget instances nest at any depth.** A `container` tag is an instance wherever it appears, so
//!   [`style_type_at`] searches containers recursively (unlike the top-level-only `style_header` case).
//! * **Native `UI*` names are still returned.** [`style_type_at`] returns a native `UI*` tag/base as a
//!   plain name (it is a real style-ish token); whether it has a *user* declaration is the server's
//!   decision — a native name simply has no declaration in any document, so it resolves to nothing,
//!   exactly as a native base does in go-to-definition. (See [`is_native_base`] — used by the server,
//!   not here: this module never classifies, it only locates.)
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.
//!
//! [`style_header`]: crate::style_index
//! [`is_native_base`]: crate::style_index::is_native_base

use crate::references::style_name_occurrences;
use crate::style_index::extract_style_defs;
use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;
use tree_sitter::Node;

/// A named style-graph node the cursor resolved to, or a subtype found in a document.
///
/// [`name`](Self::name) is the style name text; [`span`](Self::span) is the byte span of the token it
/// came from — the container tag / header name / base token under the cursor (for [`style_type_at`]),
/// or a subtype's declared-name token (for [`direct_subtypes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleRef {
    /// The style name.
    pub name: String,
    /// The byte span of the token this name came from.
    pub span: ByteSpan,
}

/// The style name the symbol under `offset` resolves to for type navigation, or `None`.
///
/// Two token shapes carry a navigable type:
/// * a **widget instance** — the `tag` of a `container`, at **any** depth (a nested `Panel`/`Button`
///   or a top-level bare container). Its tag text is the instance's type.
/// * a **top-level** `style_header`'s declared-name **or** base token. Whichever the cursor sits on is
///   returned (the name for its own declaration, the base for the type it inherits) — this is the
///   symbol the server then resolves to a declaration (typeDefinition) or lists subtypes of
///   (implementation).
///
/// A nested `Name < Base` header is an instance, not a declaration, so its name/base tokens are *not*
/// returned here (only the top-level ones are — the widget-instance path above covers the nested
/// container case). A hit anywhere else (a property, the `<`, whitespace) yields `None`. Native `UI*`
/// tags/bases are returned as names; the server decides they have no user declaration.
///
/// Half-open `[start, end)` boundary convention (matching
/// [`base_reference_at`](crate::navigation::base_reference_at)): an offset exactly at a token's end is
/// not inside it.
#[must_use]
pub fn style_type_at(source: &str, offset: usize) -> Option<StyleRef> {
    let tree = SyntaxTree::parse(source)?;
    let root = tree.root();

    // 1. A top-level `style_header`'s declared-name or base token (top-level only: a nested header is
    //    a widget instance, handled by the container search below via its tag).
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "style_header" {
            continue;
        }
        for field in ["name", "base"] {
            if let Some(node) = child.child_by_field_name(field) {
                if let Some(hit) = ref_if_inside(node, source, offset) {
                    return Some(hit);
                }
            }
        }
    }

    // 2. A `container` tag anywhere in the tree (widget instances nest at any depth).
    container_tag_at(root, source, offset)
}

/// Recursively find the `container` tag under `offset` (widget instances nest at any depth).
fn container_tag_at(node: Node<'_>, source: &str, offset: usize) -> Option<StyleRef> {
    if node.kind() == "container" {
        if let Some(tag) = node.child_by_field_name("tag") {
            if let Some(hit) = ref_if_inside(tag, source, offset) {
                return Some(hit);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(hit) = container_tag_at(child, source, offset) {
            return Some(hit);
        }
    }
    None
}

/// Build a [`StyleRef`] from `node` when `offset` falls inside its span (half-open), else `None`.
fn ref_if_inside(node: Node<'_>, source: &str, offset: usize) -> Option<StyleRef> {
    let span = SyntaxTree::span_of(node);
    if span.start <= offset && offset < span.end {
        Some(StyleRef {
            name: source[span.start..span.end].to_owned(),
            span,
        })
    } else {
        None
    }
}

/// The name span(s) where `name` is declared as a top-level style in `source` (the typeDefinition
/// target per document).
///
/// Reuses [`style_name_occurrences`]'s declaration finder, so it inherits its exact, case-sensitive,
/// top-level-only semantics. Duplicate declarations of the same name (legal in the engine) are all
/// returned. Returns empty when `source` cannot be parsed or `name` is declared nowhere here (a native
/// `UI*` name has no declaration, so it naturally yields nothing).
#[must_use]
pub fn style_declarations(source: &str, name: &str) -> Vec<ByteSpan> {
    style_name_occurrences(source, name).declarations
}

/// The styles in `source` that directly derive from `name` — every top-level `X < name` header,
/// returned as its declared name + name span (the implementation target per document).
///
/// Reuses [`extract_style_defs`] (top-level headers only) and keeps those whose base equals `name` by
/// **exact, case-sensitive** comparison, mirroring [`style_index`](crate::style_index). Returns empty
/// when `source` cannot be parsed or nothing derives from `name`.
#[must_use]
pub fn direct_subtypes(source: &str, name: &str) -> Vec<StyleRef> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    extract_style_defs(&tree)
        .into_iter()
        .filter(|def| def.base.as_deref() == Some(name))
        .map(|def| StyleRef {
            name: def.name,
            span: def.name_span,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// The substrings `source` covers for each span, for readable assertions.
    fn texts<'a>(source: &'a str, spans: &[ByteSpan]) -> Vec<&'a str> {
        spans.iter().map(|s| &source[s.start..s.end]).collect()
    }

    #[test]
    fn type_at_widget_instance_tag_returns_the_tag_name() {
        // A nested `Button` container is a widget instance; its tag is its type.
        let src = "MainWindow < UIWindow\n  Button\n    id: ok\n";
        let got = style_type_at(src, at(src, "Button")).expect("hit");
        assert_eq!(got.name, "Button");
        assert_eq!(&src[got.span.start..got.span.end], "Button");
        // A cursor in the middle of the tag is the same hit.
        assert_eq!(
            style_type_at(src, at(src, "Button") + 2).as_ref(),
            Some(&got)
        );
    }

    #[test]
    fn type_at_top_level_bare_container_tag_returns_it() {
        let src = "Panel\n  id: root\n";
        let got = style_type_at(src, at(src, "Panel")).expect("hit");
        assert_eq!(got.name, "Panel");
    }

    #[test]
    fn type_at_style_header_name_returns_the_name() {
        let src = "MainWindow < UIWindow\n";
        let got = style_type_at(src, at(src, "MainWindow")).expect("hit");
        assert_eq!(got.name, "MainWindow");
        assert_eq!(&src[got.span.start..got.span.end], "MainWindow");
    }

    #[test]
    fn type_at_style_header_base_returns_the_base() {
        let src = "MainWindow < UIWindow\n";
        let got = style_type_at(src, at(src, "UIWindow")).expect("hit");
        // The base token is returned as a name — native classification is the server's concern.
        assert_eq!(got.name, "UIWindow");
    }

    #[test]
    fn type_at_off_symbol_is_none() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        // On the `id:` value, not a type token.
        assert!(style_type_at(src, at(src, "main")).is_none());
        assert!(style_type_at("", 0).is_none());
        // Just past the base token (half-open) is not a hit.
        let end = at(src, "UIWindow") + "UIWindow".len();
        assert!(style_type_at(src, end).is_none());
    }

    #[test]
    fn declarations_find_the_decl_including_duplicates() {
        // Two top-level declarations of `Panel` (legal); both name spans are returned. The nested
        // `Panel` instance is not a declaration.
        let src = "Panel < UIWidget\nOther\n  Panel\nPanel < UIWindow\n";
        let decls = style_declarations(src, "Panel");
        assert_eq!(texts(src, &decls), ["Panel", "Panel"]);
        // The first is the top-level declaration, not the nested instance.
        assert_eq!(decls[0].start, at(src, "Panel"));
    }

    #[test]
    fn declarations_of_absent_or_native_name_are_empty() {
        let src = "Panel < UIWidget\n";
        // A native base with no user `UIWidget < …` declaration resolves to nothing.
        assert!(style_declarations(src, "UIWidget").is_empty());
        assert!(style_declarations(src, "Missing").is_empty());
    }

    #[test]
    fn direct_subtypes_finds_only_styles_whose_base_equals_the_name() {
        let src = "A < UIWidget\nB < A\nC < A\nD < B\n";
        let subs = direct_subtypes(src, "A");
        let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["B", "C"], "only direct derivations of A");
        // Each span points at the derived style's declared name.
        assert_eq!(&src[subs[0].span.start..subs[0].span.end], "B");
    }

    #[test]
    fn direct_subtypes_match_is_exact_and_case_sensitive() {
        let src = "Real < Base\nOther < base\n";
        // `base` (lowercase) is a different type than `Base`.
        let subs = direct_subtypes(src, "Base");
        let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Real"]);
    }

    #[test]
    fn direct_subtypes_ignores_nested_headers() {
        // The nested `Inner < Base` is a widget instance, not a top-level derivation.
        let src = "Outer < UIWidget\n  Inner < Base\nReal < Base\n";
        let subs = direct_subtypes(src, "Base");
        let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Real"], "only the top-level derivation counts");
    }

    #[test]
    fn direct_subtypes_of_native_base_are_still_found() {
        // Many user styles derive from a native `UI*` base; implementation on that base lists them.
        let src = "A < UIWidget\nB < UIWidget\nC < UIWindow\n";
        let subs = direct_subtypes(src, "UIWidget");
        let names: Vec<&str> = subs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
    }

    #[test]
    fn primitives_handle_unparseable_source_gracefully() {
        // Never panic on a tab-indented (scanner-rejected) document; return empty.
        let junk = "\t\t< <\n";
        assert!(direct_subtypes(junk, "X").is_empty());
        assert!(style_declarations(junk, "X").is_empty());
    }
}
