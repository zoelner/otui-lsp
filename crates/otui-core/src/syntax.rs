//! Parsing substrate: turns OTUI/OTML source text into a tree-sitter [`Tree`] and offers a small
//! set of walking helpers over it.
//!
//! This module is deliberately thin. It owns the source string alongside its parsed tree so that
//! later milestones (diagnostics, symbols, completion) can query nodes with their **byte spans**
//! and kinds without re-parsing. Everything here is protocol-agnostic — no `lsp-types`, no I/O.

use lang_api::ByteSpan;
use tree_sitter::{Node, Parser, Tree};

/// A parsed OTUI/OTML document: the original source plus its tree-sitter [`Tree`].
///
/// The source is retained because byte spans are only meaningful against the exact text that was
/// parsed, and downstream passes need to slice it.
pub struct SyntaxTree {
    source: String,
    tree: Tree,
}

impl std::fmt::Debug for SyntaxTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntaxTree")
            .field("source_len", &self.source.len())
            .field("root_kind", &self.tree.root_node().kind())
            .field("has_error", &self.tree.root_node().has_error())
            .finish()
    }
}

impl SyntaxTree {
    /// Parses `source` with the OTUI/OTML grammar.
    ///
    /// Parsing never fails for well-formed UTF-8: tree-sitter is error-tolerant and represents
    /// malformed regions with `ERROR`/`MISSING` nodes inside the tree, which the diagnostics pass
    /// harvests. `None` is only returned in the (practically unreachable) event that the grammar
    /// cannot be loaded or the parser is misconfigured.
    #[must_use]
    pub fn parse(source: &str) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_otui::language()).ok()?;
        let tree = parser.parse(source, None)?;
        Some(Self {
            source: source.to_owned(),
            tree,
        })
    }

    /// The source text this tree was parsed from.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The underlying tree-sitter tree.
    #[must_use]
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The root node of the parse tree (kind `"document"`).
    #[must_use]
    pub fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    /// Whether the tree contains any `ERROR` or `MISSING` nodes.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.root().has_error()
    }

    /// The byte span of `node`.
    #[must_use]
    pub fn span_of(node: Node<'_>) -> ByteSpan {
        ByteSpan::new(node.start_byte(), node.end_byte())
    }

    /// Visits every node in the tree in a pre-order (depth-first) walk, invoking `visit` with each
    /// node's kind and byte span. This is the generic substrate later passes build on.
    pub fn walk<F: FnMut(&str, ByteSpan)>(&self, mut visit: F) {
        let mut cursor = self.tree.walk();
        let mut recursed = true;
        loop {
            if recursed {
                let node = cursor.node();
                visit(node.kind(), Self::span_of(node));
                recursed = cursor.goto_first_child();
                continue;
            }
            if cursor.goto_next_sibling() {
                recursed = true;
            } else if !cursor.goto_parent() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_document() {
        let src = "MainWindow < UIWindow\n  id: main\n";
        let st = SyntaxTree::parse(src).expect("parse");
        assert_eq!(st.root().kind(), "document");
        assert!(!st.has_error());
        assert_eq!(st.source(), src);
    }

    #[test]
    fn walk_visits_the_root() {
        let st = SyntaxTree::parse("id: main\n").expect("parse");
        let mut kinds = Vec::new();
        st.walk(|kind, _span| kinds.push(kind.to_owned()));
        assert_eq!(kinds.first().map(String::as_str), Some("document"));
    }
}
