//! Hover Markdown for an `&tag:` **alias-property key** (spec §2.6 / §5.5): every `&` node in the
//! engine always plays **two** roles at once, and the hover must present both simultaneously — never
//! just one, and never as an either/or.
//!
//! ## Engine ground (spec §2.6)
//!
//! 1. **OTML variable/alias** — `OTMLParser::resolveVariablesRecursive`
//!    (`otmlparser.cpp:203-234`): a **root-level** `&` becomes a document-global variable (added to
//!    the document's global alias map only when `doc != nullptr`, `:209-214`); a **nested** `&` is
//!    local to its own subtree (the alias map is copied down per level, `:147`/`:234`). `$name`
//!    references resolve **file-local only** (`:106-131`) — there is no cross-file variable sharing.
//! 2. **Lua-evaluated widget-instance field** — `UIWidget::parseBaseStyle`
//!    (`uiwidgetbasestyle.cpp:262-276`): the value is evaluated as a Lua expression wrapped
//!    `__exp = (<value>)` (`luainterface.cpp:375`) and set as an instance field on the widget —
//!    **except** the §2.6 carve-out: a trimmed value that starts with a literal `#` is pushed as a
//!    **plain string**, never evaluated (`:269-270`), which is exactly what lets `&color: #ff0000`
//!    survive as a color instead of parsing as a Lua comment.
//!
//! Both roles are *always* active on every `&` node — there is no OTUI-level way to opt out of
//! either — so [`alias_hover_body`] always emits both paragraphs; only the Lua-field sentence's
//! wording depends on [`AliasHover::is_hash_literal`].
//!
//! The grammar tags the value: `alias_property`'s `value` field child is a distinct `hash_literal`
//! node for an inline `#…` carve-out, else a `lua_value` or `block_scalar`. Because the engine keys
//! off the *assembled* value's first non-whitespace char, a `block_scalar` whose body starts with a
//! bare `#` also hits the carve-out; every other `lua_value`/`block_scalar` is the evaluated case —
//! see `grammar.js`'s `alias_property` rule. A **quoted** value like `&q: '#FFFFFF'` does *not* hit
//! the carve-out (the literal `#` must be the value's own first character, not inside a string): it
//! lexes as an ordinary `lua_value`, so it gets the eval variant.
//!
//! Complements [`property_hover`](crate::property_hover) (an ordinary `key:` property) and
//! [`id_hover`](crate::id_hover) (an `id:` declaration) for the same spec §5.5 hover surface. Pure:
//! no I/O, no `lsp-types` — byte offsets in, a structured [`AliasHover`] and a Markdown `String` out.
//! The server wraps the body in an LSP `Hover` with a range over the [`alias_name`](AliasHover::name)
//! token.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;

/// A structured, protocol-agnostic description of an `&tag:` alias-property key under the cursor
/// (spec §2.6 / §5.5). The server maps [`span`](Self::span) to a range and renders
/// [`alias_hover_body`] into the hover's Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasHover {
    /// The byte span of the alias-name token (the identifier *after* the leading `&`, e.g.
    /// `primaryColor` in `&primaryColor: …` — the `&` itself is not included).
    pub span: ByteSpan,
    /// The alias name (the key text, without the leading `&`).
    pub name: String,
    /// Whether the value hits the §2.6 `#` carve-out — pushed as a plain string, never
    /// Lua-evaluated. True for an inline `hash_literal` node and for a multi-line `block_scalar`
    /// whose body starts with a bare `#` (the engine keys off the assembled value's first
    /// non-whitespace char). `false` for an ordinary `lua_value`, a non-`#` block scalar, or a
    /// *quoted* `'#…'` string, which is ordinary Lua text, not the bare carve-out.
    pub is_hash_literal: bool,
}

/// Describe the alias-property key under `offset`, or `None` when the cursor is not on one (a
/// value position, a different property kind, or an unparseable document).
///
/// Walks up from the smallest node at `offset` to the enclosing `alias_property`, mirroring
/// [`property_hover_at`](crate::property_hover::property_hover_at)'s walk — the cursor must be on
/// the `alias_name` leaf itself (which the walk naturally lands on when the cursor is inside the
/// key's text) for this to fire; a hit is not attempted on the value's span.
#[must_use]
pub fn alias_hover_at(source: &str, offset: usize) -> Option<AliasHover> {
    let tree = SyntaxTree::parse(source)?;
    let start = tree.root().descendant_for_byte_range(offset, offset)?;
    let mut node = start;
    let key = loop {
        if node.kind() == "alias_name" {
            break node;
        }
        node = node.parent()?;
    };

    let span = SyntaxTree::span_of(key);
    let name = source[span.start..span.end].to_owned();

    let alias_property = key.parent()?;
    let is_hash_literal = alias_property
        .child_by_field_name("value")
        .is_some_and(|value| match value.kind() {
            // An inline `#…` value is a distinct `hash_literal` node.
            "hash_literal" => true,
            // The engine carve-out (`uiwidgetbasestyle.cpp:269`) keys off the *assembled* value's
            // first non-whitespace char, so a block-scalar body whose content starts with a bare
            // `#` is likewise pushed as a plain string, not evaluated.
            "block_scalar" => {
                let mut cursor = value.walk();
                value
                    .children(&mut cursor)
                    .find(|c| c.kind() == "block_scalar_content")
                    .map(SyntaxTree::span_of)
                    .is_some_and(|sp| source[sp.start..sp.end].trim_start().starts_with('#'))
            }
            _ => false,
        });

    Some(AliasHover {
        span,
        name,
        is_hash_literal,
    })
}

/// Build the full Markdown `Hover` **body** for an `&name:` alias-property key (spec §2.6 / §5.5):
/// the OTML-variable paragraph (identical in both cases) followed by the Lua-field paragraph, whose
/// wording switches on `is_hash_literal`. Both paragraphs are always present — the two roles are
/// always simultaneously active on every `&` node, never an either/or.
#[must_use]
pub fn alias_hover_body(name: &str, is_hash_literal: bool) -> String {
    let otml_paragraph = format!(
        "**OTML variable** — referenced elsewhere as `${name}`. A root-level `&` is a \
         document-global variable; a nested `&` is local to its own subtree. Resolution is \
         file-local — no cross-file variable sharing."
    );
    let lua_paragraph = if is_hash_literal {
        "**Lua widget field** — the value starts with `#`, so it is pushed as a plain string, not \
         evaluated (the hex-literal carve-out — e.g. `#33AAFF` survives as a color instead of \
         parsing as a Lua comment)."
            .to_owned()
    } else {
        "**Lua widget field** — the value is evaluated as a Lua expression (`__exp = (…)`) and set \
         as a field on the widget instance."
            .to_owned()
    };
    format!("{otml_paragraph}\n\n{lua_paragraph}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    fn hover(src: &str, needle: &str) -> Option<AliasHover> {
        alias_hover_at(src, at(src, needle) + 1)
    }

    #[test]
    fn detects_the_hash_literal_carve_out() {
        let h = hover("Panel\n  &primaryColor: #33AAFF\n", "primaryColor").expect("hover");
        assert_eq!(h.name, "primaryColor");
        assert!(h.is_hash_literal);
    }

    #[test]
    fn detects_a_plain_lua_expression_value() {
        let h = hover("Panel\n  &foo: bar()\n", "foo").expect("hover");
        assert_eq!(h.name, "foo");
        assert!(!h.is_hash_literal);
    }

    #[test]
    fn a_quoted_hash_string_is_not_the_carve_out() {
        // `'#FFFFFF'` is a quoted Lua string literal (a `lua_value`), not the bare `#` carve-out
        // (`hash_literal`) — the `#` must be the value's own first character, not inside quotes.
        let h = hover("Panel\n  &color: '#FFFFFF'\n", "color").expect("hover");
        assert!(!h.is_hash_literal);
    }

    #[test]
    fn a_multiline_block_scalar_value_is_not_the_carve_out() {
        let h = hover("Panel\n  &multi: |\n    line1\n    line2\n", "multi").expect("hover");
        assert!(!h.is_hash_literal);
    }

    #[test]
    fn a_block_scalar_body_starting_with_a_bare_hash_is_the_carve_out() {
        // The engine keys off the assembled value's first non-whitespace char, so a block body that
        // begins with a bare `#` is pushed as a plain string, just like an inline `#…`.
        let h = hover("Panel\n  &note: |\n    #hello\n    world\n", "note").expect("hover");
        assert!(h.is_hash_literal);
    }

    #[test]
    fn the_span_excludes_the_leading_ampersand() {
        let src = "Panel\n  &primaryColor: #33AAFF\n";
        let h = hover(src, "primaryColor").expect("hover");
        assert_eq!(&src[h.span.start..h.span.end], "primaryColor");
    }

    #[test]
    fn a_nested_alias_is_also_detected() {
        let h = hover("Panel\n  Child\n    &nested: yes\n", "nested").expect("hover");
        assert_eq!(h.name, "nested");
        assert!(!h.is_hash_literal);
    }

    #[test]
    fn the_value_position_has_no_alias_hover() {
        // Only the key fires — a hit on the value's span is not attempted.
        assert!(hover("Panel\n  &foo: bar()\n", "bar()").is_none());
    }

    #[test]
    fn an_ordinary_property_key_has_no_alias_hover() {
        assert!(hover("Panel\n  color: red\n", "color").is_none());
    }

    // --- alias_hover_body: both meanings always present -------------------------------------------

    #[test]
    fn body_always_states_the_otml_variable_meaning() {
        for is_hash_literal in [true, false] {
            let body = alias_hover_body("primaryColor", is_hash_literal);
            assert!(body.contains("OTML variable"), "{body}");
            assert!(body.contains("$primaryColor"), "{body}");
            assert!(body.contains("document-global"), "{body}");
            assert!(body.contains("file-local"), "{body}");
        }
    }

    #[test]
    fn body_always_states_the_lua_field_meaning() {
        for is_hash_literal in [true, false] {
            let body = alias_hover_body("primaryColor", is_hash_literal);
            assert!(body.contains("Lua widget field"), "{body}");
        }
    }

    #[test]
    fn body_says_evaluated_when_not_hash_literal() {
        let body = alias_hover_body("foo", false);
        assert!(body.contains("evaluated as a Lua expression"), "{body}");
        assert!(!body.contains("plain string"), "{body}");
    }

    #[test]
    fn body_says_plain_string_when_hash_literal() {
        let body = alias_hover_body("color", true);
        assert!(body.contains("plain string, not"), "{body}");
        assert!(body.contains("carve-out"), "{body}");
        assert!(!body.contains("evaluated as a Lua expression"), "{body}");
    }
}
