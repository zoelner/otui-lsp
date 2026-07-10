//! Closed-set schema for the OTML/OTUI language (spec §2).
//!
//! This module hard-codes the small **closed sets** the OTClient engine recognizes and exposes
//! cheap, pure lookup/validation helpers over them. It is the shared vocabulary the later
//! diagnostics and completion nodes consume; it decides nothing about severity or presentation.
//!
//! Every set here was validated against the OTClient engine source (the C++ that actually parses
//! OTUI at runtime), which the project treats as the sole source of truth over any prose doc:
//!
//! * `$state` names — `Fw::translateState` (`src/framework/ui/uitranslator.cpp`).
//! * anchor edges — `Fw::translateAnchorEdge` (same file).
//! * color forms — `Color::operator>>` / `Color::Color(string_view)`
//!   (`src/framework/util/color.cpp`).
//!
//! The `@event` handler names are taken from the spec's enumerated list (§2.5); that set is
//! fork-dependent and may later be refined directly against the engine (see [`EVENTS`]).
//!
//! Everything is pure: no I/O, no `lsp-types`, ASCII/byte world only. The larger, open-ish catalogs
//! — the ~150 property names and the ~150 named colors — are **not** hand-written here: they live in
//! the generated [`crate::catalog`] module (produced by `cargo xtask gen-catalog` from the engine
//! source) and this module exposes membership helpers over them ([`is_known_property`],
//! [`is_named_color`]). This module still owns the small closed sets plus the color grammar forms.

/// The closed set of `$state` selector names (spec §2.8), exactly as recognized by the engine's
/// `Fw::translateState`. Verified against the source: **14** names, and this list is their
/// canonical lowercase spelling.
///
/// The engine lowercases and trims the incoming token before comparing, so membership is
/// case-insensitive at runtime (see [`is_known_state`]); the canonical authored form is lowercase.
///
/// Fidelity note for the diagnostics node: a `$state` token **outside** this set is *not* an engine
/// error. `translateState` returns `InvalidState`, and an invalid state simply never matches, so
/// the block silently never applies. That makes an unknown state a **hint** (a probable authoring
/// bug), never an error/warning.
pub const STATES: &[&str] = &[
    "active",
    "focus",
    "hover",
    "pressed",
    "checked",
    "disabled",
    "on",
    "first",
    "middle",
    "last",
    "alternate",
    "dragging",
    // `hidden` and `mobile` are the two less-obvious ones: `hidden` styles a widget while it is
    // hidden; `mobile` is active under the mobile/touch UI profile.
    "hidden",
    "mobile",
];

/// The closed set of anchor **edge** names (spec §2.4), from `Fw::translateAnchorEdge`. Verified
/// against the source: **6** edges. Stored in their canonical camelCase spelling; the engine
/// lowercases the token before matching, so [`is_anchor_edge`] is case-insensitive.
pub const ANCHOR_EDGES: &[&str] = &[
    "top",
    "bottom",
    "left",
    "right",
    "horizontalCenter",
    "verticalCenter",
];

/// The magic anchor **target** ids (spec §2.4): keywords that stand in for a widget instead of
/// naming a sibling/ancestor by its `id:`. `parent` is the containing widget; `next`/`prev` are the
/// adjacent siblings. (The literal value `none` is not a target — it *removes* an anchor — so it is
/// intentionally not in this set.)
pub const MAGIC_ANCHOR_TARGETS: &[&str] = &["parent", "next", "prev"];

/// The shorthand anchor keys (spec §2.4): `anchors.fill: <target>` and `anchors.centerIn: <target>`
/// expand to a full set of edge anchors against the target. These are keys, not edges, so they are
/// kept as their own small set rather than folded into [`ANCHOR_EDGES`].
pub const SHORTHAND_ANCHORS: &[&str] = &["fill", "centerIn"];

/// The closed set of accepted `display` values (spec §2.10), from the engine's `display`-style
/// dispatch. Validated against the engine source: the parser lowercases the value and compares it
/// against this fixed list; any value outside it makes the parser **throw** (`Invalid display value`),
/// so an unknown `display` value is a hard error. Membership is therefore case-insensitive (see
/// [`is_display_value`]); the canonical authored spelling is lowercase/kebab.
pub const DISPLAY_VALUES: &[&str] = &[
    "none",
    "block",
    "inline",
    "inline-block",
    "flex",
    "inline-flex",
    "grid",
    "inline-grid",
    "table",
    "table-row-group",
    "table-header-group",
    "table-footer-group",
    "table-row",
    "table-cell",
    "table-column-group",
    "table-column",
    "table-caption",
    "list-item",
    "run-in",
    "contents",
    "initial",
    "inherit",
];

/// The closed set of accepted `layout` **type** values (spec §2.10). Validated against the engine
/// source: the `layout` style resolves a type either from the leaf value (`layout: <type>`) or from a
/// nested `type:` child (`layout:` block), then compares it against this fixed list; a **non-empty**
/// value outside it makes the parser **throw** (`cannot determine layout type`), so an unknown layout
/// type is a hard error. The comparison is an exact, **case-sensitive** match (the engine does not
/// lowercase the type), so the canonical camelCase spelling is required (see [`is_layout_type`]).
pub const LAYOUT_TYPES: &[&str] = &["horizontalBox", "verticalBox", "grid", "anchor"];

/// The `border` shorthand **style** keywords, consumed (and ignored) by the engine's `border` parser
/// while it scans for a width and a color. Validated against the engine source: each is matched
/// case-insensitively and skipped, contributing neither a width nor a color. (`none`/`hidden` are
/// also the single-token "no border" spelling — see [`is_valid_border`].)
pub const BORDER_STYLE_KEYWORDS: &[&str] = &[
    "solid", "dashed", "dotted", "double", "groove", "ridge", "inset", "outset", "hidden", "none",
];

/// The `border` shorthand **named width** keywords (CSS `thin`/`medium`/`thick`), matched
/// case-insensitively by the engine's `border` parser and counting as a width. Validated against the
/// engine source.
pub const BORDER_WIDTH_KEYWORDS: &[&str] = &["thin", "medium", "thick"];

/// The known `@event` handler names (spec §2.5), completion-worthy at the `@`-key position.
///
/// Unlike the other sets in this module, this one is **fork-dependent**: it comes from the spec's
/// enumerated list rather than a single closed `translate*` switch in the engine, and different
/// OTClient forks wire up slightly different handler names. A later node may refine it directly
/// against the engine source. An `@tag:` whose name is *not* here is still valid OTML — it just
/// binds a custom Lua field — so an unknown `@event` is at most a hint, never an error.
pub const EVENTS: &[&str] = &[
    "onCreate",
    "onSetup",
    "onDestroy",
    "onIdChange",
    "onStyleApply",
    "onWidthChange",
    "onHeightChange",
    "onResize",
    "onEnabled",
    "onCheckChange",
    "onPropertyChange",
    "onGeometryChange",
    "onLayoutUpdate",
    "onFocusChange",
    "onChildFocusChange",
    "onHoverChange",
    "onTextHoverChange",
    "onVisibilityChange",
    "onDragEnter",
    "onDragLeave",
    "onDragMove",
    "onDrop",
    "onKeyText",
    "onKeyDown",
    "onKeyPress",
    "onKeyUp",
    "onMousePress",
    "onMouseRelease",
    "onMouseMove",
    "onMouseWheel",
    "onTextClick",
    "onClick",
    "onDoubleClick",
    "onTextChange",
    "onFontChange",
    "onTextAreaUpdate",
];

/// A recognized color-value form (spec §2.9), returned by [`parse_color`].
///
/// Only the two *parseable* forms are represented; named colors are handled separately by
/// [`is_named_color`] against the generated [`crate::catalog::NAMED_COLORS`] table. To decide
/// whether an arbitrary value is a valid color, use `is_valid_color(v) || is_named_color(v)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorForm {
    /// A `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` hex literal.
    Hex,
    /// A functional `rgb()`, `rgba()`, `hsl()`, or `hsla()` literal.
    Functional,
}

/// True if `name` is one of the 14 known `$state` selectors. Case-insensitive, matching the
/// engine's `tolower`+`trim` before comparison in `Fw::translateState`.
#[must_use]
pub fn is_known_state(name: &str) -> bool {
    contains_ascii_ci(STATES, name.trim())
}

/// True if `name` is one of the 6 anchor edges. Case-insensitive (the engine lowercases the token
/// in `Fw::translateAnchorEdge`).
#[must_use]
pub fn is_anchor_edge(name: &str) -> bool {
    contains_ascii_ci(ANCHOR_EDGES, name.trim())
}

/// True if `name` is a magic anchor target keyword (`parent` / `next` / `prev`). Exact match on the
/// canonical lowercase spelling.
#[must_use]
pub fn is_magic_anchor_target(name: &str) -> bool {
    MAGIC_ANCHOR_TARGETS.contains(&name)
}

/// True if `name` is a shorthand anchor key (`fill` / `centerIn`). Exact match.
#[must_use]
pub fn is_shorthand_anchor(name: &str) -> bool {
    SHORTHAND_ANCHORS.contains(&name)
}

/// True if `value` is an accepted `display` value ([`DISPLAY_VALUES`]). Case-insensitive and trimmed,
/// mirroring the engine, which lowercases the value before comparing. A `false` here is an engine
/// error: an unknown `display` value makes the style parser throw.
#[must_use]
pub fn is_display_value(value: &str) -> bool {
    contains_ascii_ci(DISPLAY_VALUES, value.trim())
}

/// True if `value` is an accepted `layout` type ([`LAYOUT_TYPES`]). **Exact**, case-sensitive match
/// (trimmed): the engine compares the resolved type verbatim, so a mis-cased `verticalbox` is not a
/// layout type and makes the parser throw.
#[must_use]
pub fn is_layout_type(value: &str) -> bool {
    LAYOUT_TYPES.contains(&value.trim())
}

/// True if `value` is a well-formed `border` shorthand, faithful to the engine's `border` parser.
///
/// The engine splits the value on spaces (dropping empty tokens) and then:
/// * a single `none`/`hidden` token (case-insensitive) is the "no border" spelling — accepted;
/// * otherwise every token is classified in order: a style keyword ([`BORDER_STYLE_KEYWORDS`]) is
///   skipped; the first token that parses as a color (hex/functional/named) supplies the color; a
///   `thin`/`medium`/`thick` keyword or any token containing a digit supplies the width;
/// * the value is valid **iff** both a width and a color were found — the engine throws
///   (`border param must include width and color`) when either is missing.
///
/// Color detection reuses the same color grammar the engine's `safe_cast<Color>` uses
/// ([`is_valid_color`]/[`is_named_color`]), so this stays consistent with the engine's own token
/// classification. An empty value yields no width and no color, so it is invalid (the engine throws).
#[must_use]
pub fn is_valid_border(value: &str) -> bool {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    if tokens.len() == 1 {
        let lower = tokens[0].to_ascii_lowercase();
        if lower == "none" || lower == "hidden" {
            return true;
        }
    }
    let mut has_width = false;
    let mut has_color = false;
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        if BORDER_STYLE_KEYWORDS.contains(&lower.as_str()) {
            continue;
        }
        // The engine tries the color cast before the width, so a token that parses as a color is a
        // color even if it also happens to contain a digit (e.g. a `#123` hex literal).
        if !has_color && (is_valid_color(token) || is_named_color(token)) {
            has_color = true;
            continue;
        }
        if !has_width {
            if BORDER_WIDTH_KEYWORDS.contains(&lower.as_str()) {
                has_width = true;
                continue;
            }
            if token.bytes().any(|b| b.is_ascii_digit()) {
                has_width = true;
            }
        }
    }
    has_width && has_color
}

/// True if `value` is a valid color for a `border-color*` sub-property: a hex/functional literal
/// ([`is_valid_color`]) or a named color ([`is_named_color`]). The engine reads these through
/// `value<Color>`, which **throws** on a value it cannot parse, so a `false` here is an engine error.
#[must_use]
pub fn is_border_color_value(value: &str) -> bool {
    let v = value.trim();
    is_valid_color(v) || is_named_color(v)
}

/// True if `name` is one of the enumerated known `@event` handler names ([`EVENTS`]). Exact match:
/// event names are Lua field names and are case-sensitive.
#[must_use]
pub fn is_known_event(name: &str) -> bool {
    EVENTS.contains(&name)
}

/// True if `name` is a named color in the generated catalog ([`crate::catalog::NAMED_COLORS`]).
/// Case-insensitive, matching the engine's `css_lookup`, which lowercases the token before
/// comparing. The table is machine-extracted from the engine's color source, so a `false` here now
/// genuinely means the name is not an engine-recognized color.
#[must_use]
pub fn is_named_color(name: &str) -> bool {
    contains_ascii_ci(crate::catalog::NAMED_COLORS, name)
}

/// True if `name` is a known OTML property tag in the generated catalog
/// ([`crate::catalog::PROPERTIES`]).
///
/// **Exact match**, not case-insensitive: the engine dispatches on `node->tag() == "..."`, an exact
/// byte compare against lowercase/kebab tag literals, so `Width` or `WIDTH` are not the `width`
/// property. The catalog stores the tags in that canonical lowercase/kebab spelling.
///
/// Fidelity note for the later diagnostics node: an unknown property name is a **hint**, never an
/// error or warning (spec §2.10) — the engine silently ignores tags it does not recognize. This
/// helper only answers membership; it decides nothing about severity.
#[must_use]
pub fn is_known_property(name: &str) -> bool {
    crate::catalog::PROPERTIES.contains(&name)
}

/// Classify a color value by its parseable form, faithful to `Color::operator>>` in the engine.
/// Returns `None` for anything that is not a well-formed hex or functional color (including named
/// colors — use [`is_named_color`] for those).
#[must_use]
pub fn parse_color(value: &str) -> Option<ColorForm> {
    let v = value.trim();
    if let Some(body) = v.strip_prefix('#') {
        return is_valid_hex_body(body).then_some(ColorForm::Hex);
    }
    if is_valid_functional(v) {
        return Some(ColorForm::Functional);
    }
    None
}

/// True if `value` is a well-formed hex or functional color literal (spec §2.9). Named colors are
/// intentionally excluded here — combine with [`is_named_color`] for a full "is this a color" check.
#[must_use]
pub fn is_valid_color(value: &str) -> bool {
    parse_color(value).is_some()
}

/// True if `name` is a syntactically valid OTML identifier — the shape the grammar's `IDENT` rule
/// accepts for a `style_name` / `property_key` / id token: `/[A-Za-z_][A-Za-z0-9_\-]*/`.
///
/// The rule is: **non-empty**, a leading ASCII letter or `_` (a digit or `-` may *not* start a
/// name), then only ASCII letters, digits, `_` or `-`. Anything containing whitespace, `:`, `.`,
/// `<`, `,`, or any other punctuation is rejected. Used to validate a proposed rename before
/// rewriting occurrences: a new name that could not be re-parsed as an identifier would silently
/// break the document, so a bad rename must be refused rather than applied.
#[must_use]
pub fn is_valid_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false; // empty is never a valid identifier
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// --- internal helpers -------------------------------------------------------------------------

/// Case-insensitive (ASCII) membership test against a `&[&str]` whose entries are the canonical
/// spellings.
fn contains_ascii_ci(set: &[&str], needle: &str) -> bool {
    set.iter().any(|entry| entry.eq_ignore_ascii_case(needle))
}

/// Faithful hex-body validation for `Color::operator>>`: after the leading `#`, a body of length 3
/// or 4 is doubled (each nibble repeated) to 6 or 8; a body already of length 6 or 8 is used as-is;
/// any other length is rejected. Every character must be an ASCII hex digit.
fn is_valid_hex_body(body: &str) -> bool {
    matches!(body.len(), 3 | 4 | 6 | 8) && body.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Faithful functional-color validation for `rgb()/rgba()/hsl()/hsla()`. Mirrors the engine: all
/// whitespace is stripped first; the function name is matched case-sensitively in lowercase; the
/// argument count must be exactly 3 (`rgb`/`hsl`) or 4 (`rgba`/`hsla`); each argument must be a
/// numeric component (optionally a percentage).
///
/// A percentage suffix is accepted for **every** form, `rgb`/`rgba` included — this is deliberate
/// fidelity, not a CSS habit. The engine's color parser reads each `rgb`/`rgba` channel through a
/// byte-or-percent helper that explicitly handles a trailing `%` (scaling `p% -> p*255/100`), so
/// `rgb(50%, 50%, 50%)` is a value the engine accepts. Rejecting `%` here to match web-CSS rules
/// would diverge from the real parser, which is our source of truth.
fn is_valid_functional(value: &str) -> bool {
    // Engine strips ALL whitespace from the token before parsing.
    let stripped: String = value.chars().filter(|c| !c.is_whitespace()).collect();

    // Longer prefixes (`rgba`/`hsla`) must be tried before their 3-char cousins.
    let Some((prefix, arity)) = ["rgba", "hsla", "rgb", "hsl"].into_iter().find_map(|name| {
        stripped
            .starts_with(&format!("{name}("))
            .then_some((name, if name.ends_with('a') { 4 } else { 3 }))
    }) else {
        return false;
    };

    // Must be exactly `name( ... )` with a non-empty, properly-closed argument list.
    let inner = stripped
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'));
    let Some(inner) = inner.filter(|s| !s.is_empty()) else {
        return false;
    };

    let parts: Vec<&str> = inner.split(',').collect();
    parts.len() == arity && parts.iter().all(|p| is_numeric_component(p))
}

/// A single functional-color argument: a decimal number (the engine uses `strtod`/`stoi`),
/// optionally suffixed with `%`. Faithful enough for validation without reproducing clamping.
fn is_numeric_component(part: &str) -> bool {
    let num = part.strip_suffix('%').unwrap_or(part);
    // Reject the non-numeric spellings `f64::from_str` would otherwise accept (`inf`, `nan`).
    if !num
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit() || b == b'+' || b == b'-' || b == b'.')
    {
        return false;
    }
    num.parse::<f64>().is_ok_and(f64::is_finite)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_set_has_exactly_fourteen_names() {
        assert_eq!(
            STATES.len(),
            14,
            "translateState recognizes exactly 14 states"
        );
    }

    #[test]
    fn known_states_are_recognized() {
        for state in STATES {
            assert!(is_known_state(state), "{state} should be known");
        }
    }

    #[test]
    fn known_state_is_case_insensitive_and_trimmed() {
        assert!(is_known_state("Hover"));
        assert!(is_known_state("HOVER"));
        assert!(is_known_state("  pressed  "));
    }

    #[test]
    fn unknown_state_is_rejected() {
        // Plausible-looking but not in the closed set.
        assert!(!is_known_state("selected"));
        assert!(!is_known_state("enabled"));
        assert!(!is_known_state(""));
    }

    #[test]
    fn anchor_edges_set_has_exactly_six() {
        assert_eq!(ANCHOR_EDGES.len(), 6);
    }

    #[test]
    fn known_anchor_edges_are_recognized_case_insensitively() {
        for edge in ANCHOR_EDGES {
            assert!(is_anchor_edge(edge));
        }
        assert!(is_anchor_edge("horizontalcenter"));
        assert!(is_anchor_edge("VerticalCenter"));
    }

    #[test]
    fn unknown_anchor_edge_is_rejected() {
        // Plausible but not an edge (`center` alone, `middle`, a shorthand key).
        assert!(!is_anchor_edge("center"));
        assert!(!is_anchor_edge("middle"));
        assert!(!is_anchor_edge("fill"));
    }

    #[test]
    fn magic_anchor_targets_recognized() {
        assert!(is_magic_anchor_target("parent"));
        assert!(is_magic_anchor_target("next"));
        assert!(is_magic_anchor_target("prev"));
        // `none` removes an anchor; it is not a target.
        assert!(!is_magic_anchor_target("none"));
        assert!(!is_magic_anchor_target("self"));
    }

    #[test]
    fn shorthand_anchors_recognized() {
        assert!(is_shorthand_anchor("fill"));
        assert!(is_shorthand_anchor("centerIn"));
        assert!(!is_shorthand_anchor("centerin")); // exact-match key
        assert!(!is_shorthand_anchor("top"));
    }

    #[test]
    fn known_events_are_recognized() {
        for event in EVENTS {
            assert!(is_known_event(event), "{event} should be known");
        }
        assert!(is_known_event("onClick"));
        assert!(is_known_event("onMouseWheel"));
    }

    #[test]
    fn unknown_or_miscased_event_is_rejected() {
        assert!(!is_known_event("onclick")); // case-sensitive
        assert!(!is_known_event("onTap")); // plausible but not enumerated
        assert!(!is_known_event(""));
    }

    #[test]
    fn hex_colors_of_each_length_parse() {
        assert_eq!(parse_color("#abc"), Some(ColorForm::Hex)); // #rgb
        assert_eq!(parse_color("#abcd"), Some(ColorForm::Hex)); // #rgba
        assert_eq!(parse_color("#aabbcc"), Some(ColorForm::Hex)); // #rrggbb
        assert_eq!(parse_color("#aabbccdd"), Some(ColorForm::Hex)); // #rrggbbaa
        assert_eq!(parse_color("#FF00FF"), Some(ColorForm::Hex)); // uppercase ok
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert!(!is_valid_color("#ab")); // len 2
        assert!(!is_valid_color("#abcde")); // len 5
        assert!(!is_valid_color("#abcdefg")); // len 7
        assert!(!is_valid_color("#gghhii")); // non-hex chars
        assert!(!is_valid_color("#")); // empty body
    }

    #[test]
    fn functional_forms_each_parse() {
        assert_eq!(parse_color("rgb(255, 0, 0)"), Some(ColorForm::Functional));
        assert_eq!(
            parse_color("rgba(255,0,0,0.5)"),
            Some(ColorForm::Functional)
        );
        assert_eq!(
            parse_color("hsl(120, 50%, 50%)"),
            Some(ColorForm::Functional)
        );
        assert_eq!(
            parse_color("hsla(120, 50%, 50%, 50%)"),
            Some(ColorForm::Functional)
        );
        // Percent components are valid even for `rgb`/`rgba` (NOT just `hsl`): the engine reads every
        // channel through a byte-or-percent helper that handles a trailing `%`. Whitespace is
        // tolerated too (the engine strips it all). Keeping this accepted is engine fidelity, not a
        // CSS convention — see `is_valid_functional`.
        assert_eq!(
            parse_color("rgb( 50% , 50% , 50% )"),
            Some(ColorForm::Functional)
        );
    }

    #[test]
    fn malformed_functional_is_rejected() {
        assert!(!is_valid_color("rgb(255, 0)")); // too few args
        assert!(!is_valid_color("rgb(1, 2, 3, 4)")); // too many for rgb
        assert!(!is_valid_color("rgba(1, 2, 3)")); // too few for rgba
        assert!(!is_valid_color("rgb(a, b, c)")); // non-numeric args
        assert!(!is_valid_color("rgb(1, 2, 3")); // unclosed
        assert!(!is_valid_color("rgb()")); // empty args
        assert!(!is_valid_color("RGB(1,2,3)")); // wrong case for function name
        assert!(!is_valid_color("frgb(1,2,3)")); // wrong prefix
    }

    #[test]
    fn named_colors_are_not_valid_color_forms() {
        // Names are not a parseable "form"; they route through is_named_color instead.
        assert!(!is_valid_color("red"));
        assert!(parse_color("red").is_none());
    }

    #[test]
    fn named_colors_recognized_case_insensitively() {
        assert!(is_named_color("aliceblue"));
        assert!(is_named_color("red"));
        assert!(is_named_color("Red"));
        assert!(is_named_color("darkRed")); // legacy engine name
        assert!(is_named_color("lightGray"));
        assert!(is_named_color("transparent")); // the `transparent` alias
        assert!(is_named_color("alpha")); // legacy engine static
        assert!(is_named_color("REBECCAPURPLE")); // a full-CSS-table name, upcased
    }

    #[test]
    fn unknown_named_color_is_rejected() {
        assert!(!is_named_color("chartreuse-ish"));
        assert!(!is_named_color("notacolor"));
        assert!(!is_named_color(""));
    }

    #[test]
    fn known_properties_recognized() {
        assert!(is_known_property("width"));
        assert!(is_known_property("color"));
        assert!(is_known_property("image-source")); // an image-* family tag
        assert!(is_known_property("text-align")); // a text-* family tag
        assert!(is_known_property("margin"));
    }

    #[test]
    fn unknown_or_miscased_property_is_rejected() {
        assert!(!is_known_property("widht")); // typo
        assert!(!is_known_property("not-a-property"));
        assert!(!is_known_property("Width")); // exact (case-sensitive) tag compare
        assert!(!is_known_property(""));
    }

    #[test]
    fn display_values_are_recognized_case_insensitively() {
        for v in DISPLAY_VALUES {
            assert!(is_display_value(v), "{v} should be a display value");
        }
        // The engine lowercases before matching, so a mis-cased value is still accepted.
        assert!(is_display_value("Flex"));
        assert!(is_display_value("TABLE-CELL"));
        assert!(is_display_value("  block  "));
    }

    #[test]
    fn unknown_display_value_is_rejected() {
        assert!(!is_display_value("blocky"));
        assert!(!is_display_value("row")); // a flex-direction value, not a display value
        assert!(!is_display_value(""));
    }

    #[test]
    fn layout_types_are_recognized_case_sensitively() {
        for v in LAYOUT_TYPES {
            assert!(is_layout_type(v), "{v} should be a layout type");
        }
        assert!(is_layout_type("  verticalBox  ")); // trimmed
                                                    // Exact, case-sensitive: the engine compares the type verbatim.
        assert!(!is_layout_type("verticalbox"));
        assert!(!is_layout_type("VerticalBox"));
    }

    #[test]
    fn unknown_layout_type_is_rejected() {
        assert!(!is_layout_type("box"));
        assert!(!is_layout_type("flex"));
        assert!(!is_layout_type(""));
    }

    #[test]
    fn valid_border_shorthands_are_accepted() {
        assert!(is_valid_border("1 red")); // width + named color
        assert!(is_valid_border("red 1")); // order-independent
        assert!(is_valid_border("2 solid #ff0000")); // style keyword skipped
        assert!(is_valid_border("thick dashed blue")); // named width + style + color
        assert!(is_valid_border("none")); // single "no border" keyword
        assert!(is_valid_border("hidden"));
        assert!(is_valid_border("HIDDEN")); // case-insensitive keyword
        assert!(is_valid_border("#abc 3")); // hex color counts as color, digit as width
    }

    #[test]
    fn invalid_border_shorthands_are_rejected() {
        assert!(!is_valid_border("red")); // color only, no width
        assert!(!is_valid_border("1")); // width only, no color
        assert!(!is_valid_border("solid")); // style keyword only
        assert!(!is_valid_border("bogus stuff")); // neither width nor color
        assert!(!is_valid_border("")); // empty -> engine throws
    }

    #[test]
    fn border_color_values_follow_the_color_grammar() {
        assert!(is_border_color_value("red"));
        assert!(is_border_color_value("#ff0000"));
        assert!(is_border_color_value("rgba(1,2,3,0.5)"));
        assert!(!is_border_color_value("notacolor"));
        assert!(!is_border_color_value("1"));
        assert!(!is_border_color_value(""));
    }

    #[test]
    fn generated_catalog_sets_are_populated() {
        // Robust `>=` bounds, not brittle equality: the exact counts move with the engine source.
        assert!(
            crate::catalog::PROPERTIES.len() >= 100,
            "expected a substantial property catalog, got {}",
            crate::catalog::PROPERTIES.len()
        );
        assert!(
            crate::catalog::NAMED_COLORS.len() >= 140,
            "expected the full CSS named-color table (~150), got {}",
            crate::catalog::NAMED_COLORS.len()
        );
    }

    #[test]
    fn valid_identifiers_are_accepted() {
        // A leading letter or `_`, then letters/digits/`_`/`-` — the grammar's IDENT shape.
        for name in [
            "Panel",
            "MyPanel",
            "_hidden",
            "a",
            "A1",
            "with-dash",
            "mix_of-3",
        ] {
            assert!(is_valid_identifier(name), "`{name}` should be valid");
        }
    }

    #[test]
    fn invalid_identifiers_are_rejected() {
        // Empty, digit/`-` start, or containing whitespace / `:` / `.` / `<` / other punctuation.
        for name in [
            "",
            "1abc",
            "-abc",
            "has space",
            "a:b",
            "a.b",
            "a<b",
            "a,b",
            "a/b",
            "café",
        ] {
            assert!(!is_valid_identifier(name), "`{name}` should be rejected");
        }
    }
}
