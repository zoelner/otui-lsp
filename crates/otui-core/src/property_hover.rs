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
/// server maps [`span`](Self::span) to a range and renders [`doc`](Self::doc) + [`value`](Self::value)
/// into Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyHover {
    /// The byte span of the property-key token the cursor is on.
    pub span: ByteSpan,
    /// The property name (the key text).
    pub name: String,
    /// A one-line behavior description for the canonical global properties (what the property does),
    /// or `None` for a known property outside the curated set. Sourced from the engine's widget
    /// style parsers; see [`PROPERTY_DOCS`].
    pub doc: Option<&'static str>,
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
        doc: property_doc(&name),
        value: classify_value(&name),
        name,
    })
}

/// The curated one-line behavior for a canonical global property, or `None` if not in the set.
#[must_use]
pub fn property_doc(name: &str) -> Option<&'static str> {
    PROPERTY_DOCS
        .binary_search_by(|(k, _)| (*k).cmp(name))
        .ok()
        .map(|i| PROPERTY_DOCS[i].1)
}

/// Curated one-line descriptions of the **canonical global** OTUI widget-style properties — what each
/// does — for hover. Sourced from the engine's widget style parsers (`parseBaseStyle` /
/// `parseImageStyle` / `parseTextStyle`, per opentibiabr's OTClient). Deliberately covers the common
/// base/image/text properties; per-widget style tags (a `UITable`'s `column-style`, `UIItem`'s
/// `item-id`, …) are not here. **Kept sorted by key** for the binary search in [`property_doc`].
pub static PROPERTY_DOCS: &[(&str, &str)] = &[
    ("background", "Filled background color drawn behind the widget."),
    ("background-color", "Filled background color drawn behind the widget."),
    ("border", "Border shorthand: a width and a color (or `none`)."),
    ("border-color", "Border color on all four edges."),
    ("checked", "The widget's checked state (checkboxes, radio-like widgets)."),
    ("clipping", "Clip the widget's children to its own rect."),
    ("color", "Foreground/text draw color."),
    ("display", "CSS-style display / layout mode (`flex`, `grid`, `table`, `none`, …); drives the flexbox/grid layout."),
    ("draggable", "Whether the widget can be dragged with the mouse."),
    ("enabled", "Whether the widget is interactive (a disabled widget is greyed and ignores input)."),
    ("fixed-size", "Lock the widget's size so a parent layout cannot resize it."),
    ("focusable", "Whether the widget can receive keyboard focus."),
    ("font", "The text font by name (resolved via `g_fonts`)."),
    ("height", "Height as a CSS-like dimension: a bare number is pixels, or `%` / `em` / `auto`."),
    ("icon", "Icon texture path (extension optional; `.png` assumed)."),
    ("icon-color", "Tint color applied to the icon."),
    ("icon-source", "Icon texture path (extension optional; `.png` assumed)."),
    ("image-clip", "Source rect (`x y w h`) clipped out of the image texture."),
    ("image-color", "Tint color multiplied into the image."),
    ("image-fixed-ratio", "Keep the image's aspect ratio when scaling."),
    ("image-repeated", "Tile (repeat) the image instead of stretching it."),
    ("image-source", "Background texture path (extension optional; `.png` assumed)."),
    ("layout", "Layout manager for the children: `verticalBox`, `horizontalBox`, `grid`, or `anchor`."),
    ("margin", "Outer spacing shorthand (1–4 values: all / v h / t h b / t r b l)."),
    ("opacity", "Opacity from 0 (transparent) to 1 (opaque)."),
    ("padding", "Inner spacing shorthand (1–4 values: all / v h / t h b / t r b l)."),
    ("phantom", "Make the widget ignore mouse events (pass-through); alias `pointer-events: none`."),
    ("pos", "Absolute position as `x y` (same coordinate space as `rect`)."),
    ("rect", "Absolute rect as `x y w h`."),
    ("rotation", "Rotation in degrees."),
    ("shader", "Named GPU shader applied when drawing the widget."),
    ("size", "Fixed size as `w h`, in pixels."),
    ("text", "The widget's displayed text."),
    ("text-align", "Text alignment: `center`, `left`, `right`, `top`, `bottom`, `topleft`, …"),
    ("text-auto-resize", "Resize the widget to fit its text on both axes."),
    ("text-wrap", "Wrap the text to the widget's width."),
    ("visible", "Whether the widget is shown."),
    ("width", "Width as a CSS-like dimension: a bare number is pixels, or `%` / `em` / `auto`."),
    ("x", "Absolute X position (same coordinate space as `rect`)."),
    ("y", "Absolute Y position (same coordinate space as `rect`)."),
];

/// Classify a property's expected value from the catalog/schema metadata. The single audited source
/// of "what value kind does this property take", shared by property hover and value completion.
#[must_use]
pub fn classify_value(name: &str) -> PropertyValueKind {
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
    fn every_property_doc_key_is_a_real_catalog_property() {
        // A curated doc for a key that is not a dispatched catalog property (e.g. the prefix-based
        // `anchors.*`, or the special-cased `id`) is dead code: `property_hover_at` gates on
        // `is_known_property`, and those forms are their own grammar nodes, never a `property_key`.
        for (key, _) in PROPERTY_DOCS {
            assert!(
                schema::is_known_property(key),
                "PROPERTY_DOCS key `{key}` is not a catalog property — it can never be hovered"
            );
        }
    }

    #[test]
    fn property_docs_are_sorted_for_binary_search() {
        // `property_doc` binary-searches PROPERTY_DOCS; an out-of-order key would silently miss.
        for pair in PROPERTY_DOCS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "PROPERTY_DOCS must be strictly sorted: `{}` !< `{}`",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn a_canonical_property_carries_its_curated_doc() {
        let h = hover("Panel\n  phantom: true\n", "phantom").expect("hover");
        assert!(
            h.doc.is_some_and(|d| d.contains("ignore mouse")),
            "phantom should carry a behavior doc, got {:?}",
            h.doc
        );
    }

    #[test]
    fn a_known_property_outside_the_curated_set_has_no_doc_but_still_hovers() {
        // `rotation` is known + curated; pick one that is known but not in PROPERTY_DOCS to prove the
        // fallback. `min-width` is a real catalog property not in the curated set.
        let h = hover("Panel\n  min-width: 10\n", "min-width").expect("hover");
        assert_eq!(h.doc, None);
        assert_eq!(h.value, PropertyValueKind::Plain);
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
