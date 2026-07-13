//! Rust binding for the OTUI/OTML tree-sitter grammar.
//!
//! OTUI/OTML is the indentation-based UI markup language used by the OTClient
//! game client. This crate exposes the compiled grammar as a
//! [`tree_sitter_language::LanguageFn`] plus a convenience [`language`]
//! constructor, and bundles the highlight/injection queries.

use tree_sitter_language::LanguageFn;

unsafe extern "C" {
    fn tree_sitter_otui() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for the OTUI/OTML grammar.
///
/// Wrap it with `tree_sitter::Language::new(LANGUAGE)` (or call [`language`]) to
/// obtain a `Language` usable with a `tree_sitter::Parser`.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_otui) };

/// Syntax-highlighting query (maps the CST to standard capture names).
pub const HIGHLIGHTS_QUERY: &str = include_str!("../../queries/highlights.scm");

/// Embedded-language injection query (injects `lua` into `@`/`&`/`!` value bodies).
pub const INJECTIONS_QUERY: &str = include_str!("../../queries/injections.scm");

/// Returns the tree-sitter `Language` for OTUI/OTML.
#[must_use]
pub fn language() -> tree_sitter::Language {
    tree_sitter::Language::new(LANGUAGE)
}

#[cfg(test)]
mod tests {
    #[test]
    fn can_load_grammar() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&super::language())
            .expect("load OTUI/OTML grammar");

        let source = "MainWindow < UIWindow\n  id: main\n  @onClick: |\n    self:hide()\n";
        let tree = parser.parse(source, None).expect("parse succeeds");
        let root = tree.root_node();

        assert_eq!(root.kind(), "document");
        assert!(!root.has_error(), "sample parses without errors");
    }
}
