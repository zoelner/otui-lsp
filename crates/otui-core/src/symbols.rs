//! Document symbols (spec §5.1): the widget-outline tree an LSP client shows for a `.otui` file.
//!
//! This pass walks the tree-sitter [`SyntaxTree`] and emits one [`DocumentSymbol`] per **widget**
//! — the instantiable/structural nodes of the grammar:
//!
//! * a `container` (a bare widget tag, e.g. `Panel`), and
//! * a `style_header` (a `Name < Base` style declaration).
//!
//! Nested widgets (containers/style headers inside a widget's block) become the parent symbol's
//! [`children`](DocumentSymbol::children), preserving containment and source order.
//!
//! ## What each symbol carries
//!
//! | field            | value                                                                   |
//! |------------------|-------------------------------------------------------------------------|
//! | `name`           | the widget's `id:` value if it has an `id_property` child; else its tag (container) or style `Name` (style_header) |
//! | `detail`         | the widget's **type**: the container tag, or the style header's `< Base` |
//! | `kind`           | [`SymbolKind::Field`] when named by an `id:`, else [`SymbolKind::Object`] |
//! | `span`           | the whole widget node                                                    |
//! | `selection_span` | the `id:` value token (preferred) or the tag / style `Name` token        |
//!
//! ## Deliberately **not** emitted as symbols
//!
//! * **Scalar properties** (`property`, `id_property`, `anchor_property`, `@event`/`&alias`/`!expr`
//!   properties, list items) — they are attributes of a widget, not structural nodes, and emitting
//!   one per line would drown the outline in noise. The `id_property` still *names* its widget; it
//!   just isn't a symbol of its own.
//! * **`$state` selector blocks** — a `state_selector` is a conditional override of its widget's
//!   properties, not a child widget, so it is not represented and its contents are not descended
//!   into. Only widget nodes are traversed for nesting, keeping the outline a faithful widget tree.

use crate::syntax::SyntaxTree;
use lang_api::{DocumentSymbol, SymbolKind};
use tree_sitter::Node;

/// Build the widget-outline forest for `source` (see the module docs).
#[must_use]
pub fn document_symbols(source: &str) -> Vec<DocumentSymbol> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    collect_widgets(tree.root(), source)
}

/// Grammar node kinds that are widgets (get a symbol and are descended into).
fn is_widget(kind: &str) -> bool {
    matches!(kind, "container" | "style_header")
}

/// Emit a symbol for every widget among `node`'s direct named children, in source order.
fn collect_widgets(node: Node<'_>, source: &str) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if is_widget(child.kind()) {
            out.push(build_symbol(child, source));
        }
    }
    out
}

/// Build the [`DocumentSymbol`] for a single widget `node` (a `container` or `style_header`),
/// recursing into its nested widgets.
fn build_symbol(node: Node<'_>, source: &str) -> DocumentSymbol {
    let span = SyntaxTree::span_of(node);

    // The widget's type token and its text (the container tag, or the style header's base).
    let (type_node, detail) = match node.kind() {
        "container" => {
            let tag = node.child_by_field_name("tag");
            (tag, tag.map(|n| slice(source, n).to_owned()))
        }
        "style_header" => {
            let base = node.child_by_field_name("base");
            (base, base.map(|n| slice(source, n).to_owned()))
        }
        // `collect_widgets` only ever calls this for widget kinds.
        _ => (None, None),
    };

    // The type-name token used as the fallback selection: the tag for a container, the `Name` for
    // a style header.
    let name_node = match node.kind() {
        "style_header" => node.child_by_field_name("name"),
        _ => type_node,
    };

    // Prefer the `id:` value: it both names the widget and provides the selection span.
    let (name, selection_span, kind) = match id_value(node) {
        Some(id) => (
            slice(source, id).to_owned(),
            SyntaxTree::span_of(id),
            SymbolKind::Field,
        ),
        None => {
            let selection = name_node.map_or(span, SyntaxTree::span_of);
            let name = name_node.map_or_else(String::new, |n| slice(source, n).to_owned());
            (name, selection, SymbolKind::Object)
        }
    };

    DocumentSymbol {
        name,
        detail,
        kind,
        span,
        selection_span,
        children: collect_widgets(node, source),
    }
}

/// The value node of `node`'s `id_property` child, if it has one with a value.
fn id_value<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "id_property" {
            return child.child_by_field_name("value");
        }
    }
    None
}

/// Slice `source` by `node`'s byte span.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-level nested document, each widget carrying an `id:`.
    const NESTED: &str = "\
MainWindow < UIWindow
  id: main
  Panel
    id: content
    Button
      id: ok
      text: Click me
";

    fn spanned<'a>(src: &'a str, sym: &DocumentSymbol) -> &'a str {
        &src[sym.span.start..sym.span.end]
    }

    fn selected<'a>(src: &'a str, sym: &DocumentSymbol) -> &'a str {
        &src[sym.selection_span.start..sym.selection_span.end]
    }

    #[test]
    fn builds_nested_widget_tree_named_by_ids() {
        let syms = document_symbols(NESTED);
        assert_eq!(syms.len(), 1, "one top-level widget");

        // MainWindow < UIWindow, named by id `main`.
        let root = &syms[0];
        assert_eq!(root.name, "main");
        assert_eq!(root.detail.as_deref(), Some("UIWindow"));
        assert_eq!(root.kind, SymbolKind::Field);
        assert_eq!(selected(NESTED, root), "main");
        // The span covers the whole style_header block (down through the Button).
        assert!(spanned(NESTED, root).starts_with("MainWindow < UIWindow"));
        assert!(spanned(NESTED, root).contains("text: Click me"));

        // Panel with id `content`.
        assert_eq!(root.children.len(), 1);
        let panel = &root.children[0];
        assert_eq!(panel.name, "content");
        assert_eq!(panel.detail.as_deref(), Some("Panel"));
        assert_eq!(panel.kind, SymbolKind::Field);
        assert_eq!(selected(NESTED, panel), "content");

        // Button with id `ok`.
        assert_eq!(panel.children.len(), 1);
        let button = &panel.children[0];
        assert_eq!(button.name, "ok");
        assert_eq!(button.detail.as_deref(), Some("Button"));
        assert_eq!(button.kind, SymbolKind::Field);
        assert_eq!(selected(NESTED, button), "ok");
        // The Button is a leaf: no nested widgets.
        assert!(button.children.is_empty());
    }

    #[test]
    fn scalar_properties_are_not_symbols() {
        // The Button has a `text:` property; it must not appear as a child symbol.
        let syms = document_symbols(NESTED);
        let button = &syms[0].children[0].children[0];
        assert!(
            button.children.is_empty(),
            "scalar properties must not be emitted as symbols"
        );
    }

    #[test]
    fn container_without_id_falls_back_to_its_tag() {
        let src = "\
Panel
  Label
    text: Hi
";
        let syms = document_symbols(src);
        assert_eq!(syms.len(), 1);
        let panel = &syms[0];
        // No id: named by its tag, kind Object, detail is the same tag (its type).
        assert_eq!(panel.name, "Panel");
        assert_eq!(panel.detail.as_deref(), Some("Panel"));
        assert_eq!(panel.kind, SymbolKind::Object);
        assert_eq!(selected(src, panel), "Panel");

        assert_eq!(panel.children.len(), 1);
        let label = &panel.children[0];
        assert_eq!(label.name, "Label");
        assert_eq!(label.detail.as_deref(), Some("Label"));
        assert_eq!(label.kind, SymbolKind::Object);
        assert!(label.children.is_empty());
    }

    #[test]
    fn mixes_id_named_and_tag_named_siblings_in_order() {
        let src = "\
Panel
  id: root
  Label
  Button
    id: ok
";
        let syms = document_symbols(src);
        let root = &syms[0];
        assert_eq!(root.name, "root");
        assert_eq!(root.kind, SymbolKind::Field);
        assert_eq!(root.children.len(), 2);
        // Source order preserved: Label (by tag) then Button (by id).
        assert_eq!(root.children[0].name, "Label");
        assert_eq!(root.children[0].kind, SymbolKind::Object);
        assert_eq!(root.children[1].name, "ok");
        assert_eq!(root.children[1].kind, SymbolKind::Field);
    }

    #[test]
    fn state_selector_blocks_are_not_widgets() {
        // A `$state:` block is a property override, not a child widget: no symbol, not descended.
        let src = "\
Button
  id: ok
  $on:
    color: red
";
        let syms = document_symbols(src);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "ok");
        assert!(
            syms[0].children.is_empty(),
            "state selector must not appear in the outline"
        );
    }

    #[test]
    fn empty_source_has_no_symbols() {
        assert!(document_symbols("").is_empty());
    }
}
