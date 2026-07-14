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
//! * any other **known** catalog property → no extra value-kind sentence, but still says whether an
//!   invalid value is rejected or silently ignored (see [`documentation_body`]).
//!
//! Every **known** catalog property always says whether the engine actually *validates* its value:
//! `display`, `layout`, `border`, an `anchors.<edge>`/shorthand key, and every color-typed property
//! ([`catalog::COLOR_PROPERTIES`] — the engine's `Color(node->value())` throws on an unparseable
//! value, confirmed by `diagnostics::check_property_value`'s color-property branch) are the
//! **validating** family: a malformed value is a hard engine error. Every other known property either
//! applies cleanly or is silently ignored if the value doesn't parse the way the code expects.
//!
//! An **unknown** key yields `None` (no hover): the `unknown-property` diagnostic already tells the
//! author it has no effect, and the widget-aware Lua-added properties are a separate concern (a later
//! slice can describe them via the workspace indexes, mirroring `completion`). Pure: byte offsets in,
//! a structured [`PropertyHover`] out — the server formats it into Markdown.
//!
//! [`documentation_body`] is the single shared formatter for "what does this property mean" markdown
//! *body* text (everything but a `**\`name\`**` header): both the completion module (a global
//! property-key item's `documentation`) and the server's property-key hover render call it, so the
//! two surfaces can never drift apart on wording.

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
    (
        "background",
        "Filled background color drawn behind the widget.",
    ),
    (
        "background-color",
        "Filled background color drawn behind the widget.",
    ),
    (
        "border",
        "Border shorthand: a width and a color (or `none`).",
    ),
    ("border-color", "Border color on all four edges."),
    (
        "checked",
        "The widget's checked state (checkboxes, radio-like widgets).",
    ),
    ("clipping", "Clip the widget's children to its own rect."),
    ("color", "Foreground/text draw color."),
    (
        "display",
        "CSS-style display / layout mode (`flex`, `grid`, `table`, `none`, …); drives the flexbox/grid layout.",
    ),
    (
        "draggable",
        "Whether the widget can be dragged with the mouse.",
    ),
    (
        "enabled",
        "Whether the widget is interactive (a disabled widget is greyed and ignores input).",
    ),
    (
        "fixed-size",
        "Lock the widget's size so a parent layout cannot resize it.",
    ),
    (
        "focusable",
        "Whether the widget can receive keyboard focus.",
    ),
    ("font", "The text font by name (resolved via `g_fonts`)."),
    (
        "height",
        "Height as a CSS-like dimension: a bare number is pixels, or `%` / `em` / `auto`.",
    ),
    (
        "icon",
        "Icon texture path (extension optional; `.png` assumed).",
    ),
    ("icon-color", "Tint color applied to the icon."),
    (
        "icon-source",
        "Icon texture path (extension optional; `.png` assumed).",
    ),
    (
        "image-clip",
        "Source rect (`x y w h`) clipped out of the image texture.",
    ),
    ("image-color", "Tint color multiplied into the image."),
    (
        "image-fixed-ratio",
        "Keep the image's aspect ratio when scaling.",
    ),
    (
        "image-repeated",
        "Tile (repeat) the image instead of stretching it.",
    ),
    (
        "image-source",
        "Background texture path (extension optional; `.png` assumed).",
    ),
    (
        "layout",
        "Layout manager for the children: `verticalBox`, `horizontalBox`, `grid`, or `anchor`.",
    ),
    (
        "margin",
        "Outer spacing shorthand (1–4 values: all / v h / t h b / t r b l).",
    ),
    ("opacity", "Opacity from 0 (transparent) to 1 (opaque)."),
    (
        "padding",
        "Inner spacing shorthand (1–4 values: all / v h / t h b / t r b l).",
    ),
    (
        "phantom",
        "Make the widget ignore mouse events (pass-through); alias `pointer-events: none`.",
    ),
    (
        "pos",
        "Absolute position as `x y` (same coordinate space as `rect`).",
    ),
    ("rect", "Absolute rect as `x y w h`."),
    ("rotation", "Rotation in degrees."),
    (
        "shader",
        "Named GPU shader applied when drawing the widget.",
    ),
    ("size", "Fixed size as `w h`, in pixels."),
    ("text", "The widget's displayed text."),
    (
        "text-align",
        "Text alignment: `center`, `left`, `right`, `top`, `bottom`, `topleft`, …",
    ),
    (
        "text-auto-resize",
        "Resize the widget to fit its text on both axes.",
    ),
    ("text-wrap", "Wrap the text to the widget's width."),
    ("visible", "Whether the widget is shown."),
    (
        "width",
        "Width as a CSS-like dimension: a bare number is pixels, or `%` / `em` / `auto`.",
    ),
    (
        "x",
        "Absolute X position (same coordinate space as `rect`).",
    ),
    (
        "y",
        "Absolute Y position (same coordinate space as `rect`).",
    ),
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

/// The shared Markdown documentation **body** for `name` — everything worth saying about it except
/// a `**\`name\`**` header, which every caller prepends itself. The single source both the completion
/// module (a global property-key item's `documentation`) and the server's property-key hover render
/// build on, so the two surfaces never diverge.
///
/// `name` may be either:
/// * a global catalog **property** key ([`schema::is_known_property`]) — ALWAYS `Some`, never `None`,
///   even for an uncurated `Plain` property with no value-kind sentence (e.g. `min-width`): a known
///   property always has at least the validation note (see below) to say. In order, the body is:
///   1. the curated one-line behavior ([`property_doc`]), when present;
///   2. a value-kind sentence from [`classify_value`], when the kind warrants one: `Color` → "Takes a
///      color.", `AssetPath` → "Takes a texture path (extension optional).", `Border` → the
///      width-and-color shorthand note, `Enum` → the `"One of: `a`, `b`, …"` line — appended
///      unconditionally, even alongside a curated sentence covering the same ground, since the value
///      kind is its own distinct fact from the prose; `Plain` contributes nothing here;
///   3. a **validation note** — [`is_validating_property`] says whether the engine hard-validates
///      this property's value (throws on a malformed one) or silently ignores an unparseable value.
/// * an `anchors.<edge>` **edge** key or shorthand key ([`schema::is_anchor_edge`] /
///   [`schema::is_shorthand_anchor`], e.g. `top`, `fill`) — these are a distinct grammar node (not a
///   `property_key`), so they bypass the catalog entirely and get their own short explanation. Used
///   today by the completion module's `anchors.<edge>`/shorthand key items; `property_hover_at` does
///   not (yet) resolve a hover on an anchor-edge token, so this branch is exercised only through
///   completion for now.
///
/// `None` only when `name` is neither — an unknown/misspelled key that resolves to nothing (the
/// `unknown-property` diagnostic already covers that case as a hint).
#[must_use]
pub fn documentation_body(name: &str) -> Option<String> {
    if schema::is_anchor_edge(name) || schema::is_shorthand_anchor(name) {
        return Some(anchor_edge_body(name));
    }
    if !schema::is_known_property(name) {
        return None;
    }
    let value = classify_value(name);
    let mut parts: Vec<String> = Vec::new();
    if let Some(doc) = property_doc(name) {
        parts.push(doc.to_owned());
    }
    match &value {
        PropertyValueKind::Color => parts.push("Takes a color.".to_owned()),
        PropertyValueKind::AssetPath => {
            parts.push("Takes a texture path (extension optional).".to_owned());
        }
        PropertyValueKind::Border => {
            parts.push("A border shorthand: a width and a color (or `none`).".to_owned());
        }
        PropertyValueKind::Enum { values } => {
            let list = values
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("One of: {list}"));
        }
        // Nothing extra beyond the curated doc (if any) for a plainly-typed property.
        PropertyValueKind::Plain => {}
    }
    parts.push(if is_validating_property(name) {
        "OTClient rejects an invalid value.".to_owned()
    } else {
        "An unrecognized value is silently ignored.".to_owned()
    });
    Some(parts.join("\n\n"))
}

/// Whether the engine hard-validates `name`'s value — throws a real `OTMLException` on a malformed
/// one — rather than applying cleanly or silently ignoring an unparseable value. Mirrors
/// `diagnostics::check_property_value`'s dispatch *exactly* (the actual `INVALID_PROPERTY_VALUE`
/// source of truth): `display`, `layout`, `border`, and every color-typed property
/// ([`catalog::COLOR_PROPERTIES`] — `Color(node->value())` throws on an unparseable value, not just
/// for `border-color`; see that module's `check_property_value` doc comment). The anchor-key case
/// (`anchors.*`) is handled by the separate early-return branch in [`documentation_body`], so it
/// never reaches this function — see [`anchor_edge_body`] for its own validation note.
#[must_use]
fn is_validating_property(name: &str) -> bool {
    matches!(name, "display" | "layout" | "border") || catalog::COLOR_PROPERTIES.contains(&name)
}

/// The documentation body for an `anchors.<edge>` edge key or shorthand key — `edge` is the bare
/// spelling completion offers (`top`, `fill`, …), not the dotted `anchors.top` form. The two
/// shorthands ([`schema::SHORTHAND_ANCHORS`]) get their own wording (they anchor more than one edge
/// at once); every other name reaching here is a genuine [`schema::ANCHOR_EDGES`] member.
///
/// States both facts spec §2.4 establishes about `anchors.*` — the other validating family besides
/// [`is_validating_property`]'s catalog properties:
/// * **validation** — an unrecognized edge/shorthand name is rejected: `check_anchor_property` in
///   `diagnostics.rs` flags it `INVALID_ANCHOR_EDGE`, a hard `Severity::Error`;
/// * **resolution** — `UIAnchor::getHookedWidget` → `parentWidget->getChildById(targetId)` searches
///   only the parent's **direct children**, so the *value* resolves to a magic target
///   (`parent`/`next`/`prev`) or a **direct sibling**'s `id:` value only. An ancestor or a
///   non-sibling id is not a parse error, but it silently fails to resolve at layout time, so this is
///   called out explicitly rather than left as a vague "a target widget".
fn anchor_edge_body(edge: &str) -> String {
    const VALIDATION_NOTE: &str = "OTClient rejects an invalid anchor edge.";
    const RESOLUTION_NOTE: &str = "The target is a direct sibling widget's `id:` value, or a magic \
        pseudo-target (`parent`, `next`, `prev`); an ancestor or non-sibling id silently fails to \
        resolve at layout time.";
    match edge {
        "fill" => format!(
            "Anchors shorthand: anchors all four edges to the target, filling it \
             (`anchors.fill: <target>`). {VALIDATION_NOTE} {RESOLUTION_NOTE}"
        ),
        "centerIn" => format!(
            "Anchors shorthand: anchors this widget's center to the target's center \
             (`anchors.centerIn: <target>`). {VALIDATION_NOTE} {RESOLUTION_NOTE}"
        ),
        _ => format!(
            "Anchors this widget's `{edge}` edge to a target (`anchors.{edge}: <target>`). \
             {VALIDATION_NOTE} {RESOLUTION_NOTE}"
        ),
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

    // --- documentation_body: the shared completion/hover formatter -------------------------------

    #[test]
    fn documentation_body_uses_the_curated_doc_for_a_canonical_property() {
        // `width` is curated and Plain-valued (non-validating): curated prose + the
        // silently-ignored note, no value-kind sentence.
        let body = documentation_body("width").expect("width has a doc");
        assert!(body.contains("dimension"), "{body}");
        assert!(body.contains("silently ignored"), "{body}");
    }

    #[test]
    fn documentation_body_appends_one_of_for_a_validating_enum_property() {
        // `display` is curated, Enum-valued AND one of the validating properties: curated prose +
        // "One of: ..." + the "rejects an invalid value" note.
        let body = documentation_body("display").expect("display has a doc");
        assert!(body.contains("One of:"), "{body}");
        for value in schema::DISPLAY_VALUES {
            assert!(body.contains(&format!("`{value}`")), "{body}");
        }
        assert!(body.contains("rejects an invalid value"), "{body}");
    }

    #[test]
    fn documentation_body_covers_the_border_shorthand_and_says_it_validates() {
        let body = documentation_body("border").expect("border has a doc");
        assert!(body.to_lowercase().contains("shorthand"), "{body}");
        assert!(body.contains("rejects an invalid value"), "{body}");
    }

    #[test]
    fn documentation_body_says_an_uncurated_color_property_validates() {
        // `border-color-bottom` is color-typed but has no curated one-liner: the value-kind sentence
        // substitutes for the missing prose, and — being color-typed — it also validates (the
        // engine's `Color(node->value())` throws on an unparseable value; see
        // `diagnostics::check_property_value`'s color-property branch, not just `border` itself).
        let body =
            documentation_body("border-color-bottom").expect("border-color-bottom has a doc");
        assert!(body.contains("Takes a color."), "{body}");
        assert!(body.contains("rejects an invalid value"), "{body}");
    }

    #[test]
    fn documentation_body_is_always_some_for_a_known_property_even_uncurated_and_plain() {
        // `min-width` is known, Plain-valued and uncurated: no curated prose, no value-kind
        // sentence — but it is still a KNOWN property, so the body is never `None`; it carries at
        // least the validation note. `min-width` is not one of the validating families, so an
        // invalid value is silently ignored, not rejected.
        let body = documentation_body("min-width").expect("a known property always has a body");
        assert!(body.contains("silently ignored"), "{body}");
        assert!(!body.contains("rejects an invalid value"), "{body}");
    }

    #[test]
    fn documentation_body_is_none_for_an_unknown_name() {
        // An unknown/misspelled key is not a known property at all — the only case that stays
        // `None` (the `unknown-property` diagnostic already covers it as a hint, spec §2.10).
        assert_eq!(documentation_body("not-a-real-property"), None);
    }

    #[test]
    fn documentation_body_anchor_edge_states_validation_and_direct_sibling_resolution() {
        // Spec §2.4: an unrecognized edge is rejected (INVALID_ANCHOR_EDGE, a hard error), AND
        // `getChildById` resolves only the parent's direct children, so the target is a direct
        // sibling id or a magic pseudo-target — never an ancestor or non-sibling id. Both facts are
        // distinct and both must be stated, mirroring how every other validating property gets an
        // explicit "rejects an invalid value" sentence.
        let body = documentation_body("top").expect("anchor edge has a doc");
        assert!(body.contains("edge"), "{body}");
        assert!(body.contains("anchors.top"), "{body}");
        assert!(body.contains("rejects an invalid"), "{body}");
        assert!(body.contains("direct sibling"), "{body}");
        assert!(
            body.contains("parent") && body.contains("next") && body.contains("prev"),
            "{body}"
        );
        assert!(body.contains("ancestor"), "{body}");
    }

    #[test]
    fn documentation_body_anchor_shorthands_also_state_validation_and_direct_sibling_resolution() {
        let fill = documentation_body("fill").expect("fill has a doc");
        assert!(fill.to_lowercase().contains("all four edges"), "{fill}");
        assert!(fill.contains("rejects an invalid"), "{fill}");
        assert!(fill.contains("direct sibling"), "{fill}");

        let center_in = documentation_body("centerIn").expect("centerIn has a doc");
        assert!(center_in.to_lowercase().contains("center"), "{center_in}");
        assert!(center_in.contains("rejects an invalid"), "{center_in}");
        assert!(center_in.contains("direct sibling"), "{center_in}");
    }

    #[test]
    fn is_validating_property_matches_diagnostics_check_property_value() {
        // The exact set `diagnostics::check_property_value` dispatches `INVALID_PROPERTY_VALUE` for:
        // display, layout, border, and every color-typed property — mirrored here 1:1.
        assert!(is_validating_property("display"));
        assert!(is_validating_property("layout"));
        assert!(is_validating_property("border"));
        for &color_prop in catalog::COLOR_PROPERTIES {
            assert!(
                is_validating_property(color_prop),
                "{color_prop} is color-typed and must validate"
            );
        }
        // A non-validating, unrelated known property.
        assert!(!is_validating_property("width"));
        assert!(!is_validating_property("min-width"));
    }
}
