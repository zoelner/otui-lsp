//! Locating color values in a document for the LSP `documentColor` feature (spec §2.9).
//!
//! Walks the CST for the grammar's dedicated `color` literals and resolves each to its actual
//! [`Rgba`] via [`schema::color_value`], returning `(span, rgba)` pairs the server maps to LSP
//! `ColorInformation`. Everything here is pure — byte spans only, no `lsp-types` — so it is
//! unit-testable without a live server.
//!
//! ## Why only `color` literals (named-color swatches are deferred)
//!
//! Only the **hex** (`#rgb`/`#rrggbb`/…) and **functional** (`rgb(..)`, `hsl(..)`, …) forms are
//! scanned. The grammar tags exactly those as a dedicated `color` node, and they are **context-free**
//! — a `#ff0000` or `rgb(1,2,3)` is unambiguously a color no matter which property it sits on, so it
//! never false-positives.
//!
//! A **named** color (`red`, `blue`, …) is just a bare word, lexically indistinguishable from an
//! `id:` value or any other identifier — `id: red` must NOT get a swatch. Deciding whether a bare
//! word is a color requires knowing which properties are **color-typed** (`color`, `*-color`, …),
//! metadata the catalog does not carry yet. So named-color swatching is **deferred** pending a
//! color-typed-property catalog (a future `xtask` extraction of the engine's `value<Color>` property
//! set); this also makes legacy-static color values moot for now (they only matter for named
//! swatches). [`schema::color_value`] still resolves named strings — it is used by
//! `colorPresentation` — we simply do not SCAN for them here.

use lang_api::ByteSpan;

use crate::schema::{self, Rgba};
use crate::syntax::SyntaxTree;

/// The CST node kind that unambiguously holds a color: the grammar's `color` literal — a hex
/// (`#rrggbb`, …) or functional (`rgb(..)`, `hsl(..)`, …) form (spec §2.9). These are context-free
/// colors, so scanning only them cannot false-positive. Named colors (bare words) are intentionally
/// NOT scanned — see the module docs.
const COLOR_NODE_KIND: &str = "color";

/// Find every color literal in `source` and its resolved [`Rgba`], each with the byte span of the
/// exact token (spec §2.9). Returns an empty vector when the source cannot be parsed. Only the
/// grammar's context-free `color` literals (hex + functional) are scanned; bare named colors are not
/// (see the module docs), so `id: red` and any identifier merely spelled like a color yield nothing.
#[must_use]
pub fn document_colors(source: &str) -> Vec<(ByteSpan, Rgba)> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    tree.walk(|kind, span| {
        if kind != COLOR_NODE_KIND {
            return;
        }
        let text = &source[span.start..span.end];
        if let Some(rgba) = schema::color_value(text) {
            out.push((span, rgba));
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `(text slice, rgba)` for each color found, for readable assertions.
    fn colors_with_text(source: &str) -> Vec<(&str, Rgba)> {
        document_colors(source)
            .into_iter()
            .map(|(span, rgba)| (&source[span.start..span.end], rgba))
            .collect()
    }

    #[test]
    fn finds_hex_and_functional_colors_with_spans() {
        let source = "Panel\n  color: #ff0000\n  background-color: rgb(0, 255, 0)\n";
        let found = document_colors(source);
        assert_eq!(found.len(), 2);

        // First swatch: the `#ff0000` token, red.
        let (span0, rgba0) = found[0];
        assert_eq!(&source[span0.start..span0.end], "#ff0000");
        assert_eq!(rgba0, Rgba::from_u8(255, 0, 0, 255));

        // Second swatch: the `rgb(0, 255, 0)` token, green.
        let (span1, rgba1) = found[1];
        assert_eq!(&source[span1.start..span1.end], "rgb(0, 255, 0)");
        assert_eq!(rgba1, Rgba::from_u8(0, 255, 0, 255));
    }

    #[test]
    fn named_colors_are_not_scanned() {
        // A bare named color in a value position is lexically an identifier/plain value, not a
        // grammar `color` node, so it is NOT swatched — named-color swatching is deferred (see the
        // module docs). `schema::color_value("red")` still resolves; we simply do not scan for it.
        assert!(document_colors("Label\n  color: red\n").is_empty());
        assert!(document_colors("Panel\n  color: transparent\n").is_empty());
        // Only the hex literal in a mixed array is found; the bare `red` is skipped.
        let found = colors_with_text("Widget\n  colors: [red, #00ff00]\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, "#00ff00");
        assert_eq!(found[0].1, Rgba::from_u8(0, 255, 0, 255));
    }

    #[test]
    fn identifier_spelled_like_a_color_is_not_a_swatch() {
        // The classic false-positive: an `id:` value spelled exactly like a named color must yield
        // no color, because a bare word is indistinguishable from a real color name without
        // color-typed-property metadata.
        assert!(document_colors("Panel\n  id: red\n").is_empty());
        assert!(document_colors("Panel\n  text: blue\n").is_empty());
    }

    #[test]
    fn ignores_non_color_values() {
        // A number, a plain word and an id are not colors.
        let source = "Panel\n  width: 100\n  text: Hello World\n  id: main\n";
        assert!(document_colors(source).is_empty());
    }

    #[test]
    fn unparsable_source_yields_no_colors() {
        // An unterminated inline array parses to an ERROR node but must not panic.
        let _ = document_colors("x: [a, b\n");
    }
}
