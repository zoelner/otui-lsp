//! Hover descriptions for a **property key** (spec §5.5): "what does this style property expect?".
//!
//! Complements [`hover`](crate::hover), which describes style *names* and inheritance bases. This
//! module answers the same question for the `key` of a `key: value` property — enough for an author
//! to hover a tag like `image-source` or `display` and understand what it is and what value it takes.
//!
//! The description is derived entirely from the metadata the engine catalog already carries — no
//! hand-authored prose — so it stays in lock-step with the generated catalog:
//!
//! * a **color** property ([`catalog::COLOR_PROPERTIES`]) → takes a color;
//! * an **asset-path** property ([`schema::PATH_PROPERTIES`]) → a texture path;
//! * `display` / `layout` → one of a fixed value set ([`schema::DISPLAY_VALUES`] /
//!   [`schema::LAYOUT_TYPES`]);
//! * `border` → the width-and-color shorthand;
//! * any other **known** catalog property → a plain "OTUI style property" note.
//!
//! An **unknown** key yields `None` (no hover): the `unknown-property` diagnostic already tells the
//! author it has no effect, and the widget-aware Lua-added properties are a separate concern (a later
//! slice can describe them via the workspace indexes, mirroring `completion`). Pure: byte offsets in,
//! a structured [`PropertyHover`] out — the server formats it into Markdown.

use crate::catalog;
use crate::schema;
use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;

/// A structured, protocol-agnostic description of a property key under the cursor (spec §5.5). The
/// server maps [`span`](Self::span) to a range and renders [`value`](Self::value) into Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyHover {
    /// The byte span of the property-key token the cursor is on.
    pub span: ByteSpan,
    /// The property name (the key text).
    pub name: String,
    /// What value the property expects.
    pub value: PropertyValueKind,
}

/// The value a property expects, derived from the catalog/schema metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyValueKind {
    /// A color value (hex / functional / named), per [`catalog::COLOR_PROPERTIES`].
    Color,
    /// An asset path (a texture), per [`schema::PATH_PROPERTIES`]. The extension is optional (the
    /// engine assumes `.png`).
    AssetPath,
    /// One of a fixed set of keyword values (e.g. `display`, `layout`).
    Enum {
        /// The accepted values, in canonical order.
        values: &'static [&'static str],
    },
    /// The `border` shorthand: a width and a color (or the `none` keyword).
    Border,
    /// A known catalog property with no specially-typed value.
    Plain,
}

/// Describe the property key under `offset`, or `None` when the cursor is not on a **known** property
/// key (an unknown key, a value position, a non-property token, or an unparseable document).
#[must_use]
pub fn property_hover_at(source: &str, offset: usize) -> Option<PropertyHover> {
    let tree = SyntaxTree::parse(source)?;
    // The smallest node at the cursor. On a property key this is the `property_key` leaf itself; on a
    // value or elsewhere it is some other node, and the walk below finds no `property_key` ancestor
    // (the key is a sibling of the value, never its ancestor) → `None`.
    let start = tree.root().descendant_for_byte_range(offset, offset)?;
    let mut node = start;
    let key = loop {
        if node.kind() == "property_key" {
            break node;
        }
        node = node.parent()?;
    };

    let span = SyntaxTree::span_of(key);
    let name = source[span.start..span.end].to_owned();
    if !schema::is_known_property(&name) {
        return None;
    }
    Some(PropertyHover {
        span,
        value: classify_value(&name),
        name,
    })
}

/// Classify a known property's expected value from the catalog/schema metadata.
fn classify_value(name: &str) -> PropertyValueKind {
    if catalog::COLOR_PROPERTIES.contains(&name) {
        return PropertyValueKind::Color;
    }
    if schema::PATH_PROPERTIES.contains(&name) {
        return PropertyValueKind::AssetPath;
    }
    match name {
        "display" => PropertyValueKind::Enum {
            values: schema::DISPLAY_VALUES,
        },
        "layout" => PropertyValueKind::Enum {
            values: schema::LAYOUT_TYPES,
        },
        "border" => PropertyValueKind::Border,
        _ => PropertyValueKind::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    fn hover(src: &str, needle: &str) -> Option<PropertyHover> {
        property_hover_at(src, at(src, needle) + 1)
    }

    #[test]
    fn describes_a_color_property() {
        let h = hover("Panel\n  color: red\n", "color").expect("hover");
        assert_eq!(h.name, "color");
        assert_eq!(h.value, PropertyValueKind::Color);
    }

    #[test]
    fn describes_an_asset_path_property() {
        let h = hover("Panel\n  image-source: /images/ui/x\n", "image-source").expect("hover");
        assert_eq!(h.value, PropertyValueKind::AssetPath);
    }

    #[test]
    fn describes_the_display_enum() {
        let h = hover("Panel\n  display: flex\n", "display").expect("hover");
        assert_eq!(
            h.value,
            PropertyValueKind::Enum {
                values: schema::DISPLAY_VALUES
            }
        );
    }

    #[test]
    fn describes_the_layout_enum() {
        let h = hover("Panel\n  layout: verticalBox\n", "layout").expect("hover");
        assert_eq!(
            h.value,
            PropertyValueKind::Enum {
                values: schema::LAYOUT_TYPES
            }
        );
    }

    #[test]
    fn describes_the_border_shorthand() {
        let h = hover("Panel\n  border: 2 solid red\n", "border: ").expect("hover");
        assert_eq!(h.value, PropertyValueKind::Border);
    }

    #[test]
    fn describes_a_plain_known_property() {
        let h = hover("Panel\n  width: 10\n", "width").expect("hover");
        assert_eq!(h.value, PropertyValueKind::Plain);
    }

    #[test]
    fn the_span_covers_exactly_the_key_token() {
        let src = "Panel\n  width: 10\n";
        let h = hover(src, "width").expect("hover");
        assert_eq!(&src[h.span.start..h.span.end], "width");
    }

    #[test]
    fn an_unknown_property_has_no_hover() {
        assert!(hover("Panel\n  widht: 10\n", "widht").is_none());
    }

    #[test]
    fn the_value_position_has_no_property_hover() {
        // Hovering the value, not the key, is not a property-key hover.
        assert!(hover("Panel\n  color: red\n", "red").is_none());
    }

    #[test]
    fn a_non_property_token_has_no_hover() {
        // The style header name / base is a style hover, not a property hover.
        assert!(property_hover_at("Panel < UIWidget\n", 2).is_none());
    }
}
