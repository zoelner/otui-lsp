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
//! Everything is pure: no I/O, no `lsp-types`, ASCII/byte world only. The ~100 property names are
//! deliberately **not** here — that large open-ish catalog belongs to the xtask extraction node;
//! this module is only the small closed sets plus the color grammar forms.

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

/// A **partial**, explicitly-incomplete set of named colors (lowercase), covering the legacy
/// engine-specific names (`alpha`, `darkRed`, `lightGray`, …) plus the most common CSS names.
///
// TODO(xtask): full named-color table extracted from color.cpp (the ~150-entry CSS table plus the
// `transparent` alias). This partial set is only the obvious/legacy names the spec calls out; the
// xtask catalog node replaces it with the complete list. Keep [`is_named_color`] as the seam.
const NAMED_COLORS_PARTIAL: &[&str] = &[
    // Legacy engine-specific names (Color:: statics), lowercased for case-insensitive lookup.
    "alpha",
    "black",
    "white",
    "red",
    "darkred",
    "green",
    "darkgreen",
    "blue",
    "darkblue",
    "pink",
    "darkpink",
    "yellow",
    "darkyellow",
    "teal",
    "darkteal",
    "gray",
    "darkgray",
    "lightgray",
    "orange",
    // A handful of the most common CSS names + the `transparent` alias.
    "transparent",
    "cyan",
    "magenta",
    "lime",
    "navy",
    "purple",
    "silver",
    "maroon",
    "olive",
    "aqua",
    "fuchsia",
    "gold",
    "grey",
];

/// A recognized color-value form (spec §2.9), returned by [`parse_color`].
///
/// Only the two *parseable* forms are represented; named colors are handled separately by
/// [`is_named_color`] because their catalog is (for now) partial. To decide whether an arbitrary
/// value is a valid color, use `is_valid_color(v) || is_named_color(v)`.
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

/// True if `name` is one of the enumerated known `@event` handler names ([`EVENTS`]). Exact match:
/// event names are Lua field names and are case-sensitive.
#[must_use]
pub fn is_known_event(name: &str) -> bool {
    EVENTS.contains(&name)
}

/// True if `name` is a named color in the (partial) catalog ([`NAMED_COLORS_PARTIAL`]).
/// Case-insensitive. See the `TODO(xtask)` note: this is a deliberate partial set behind a stable
/// seam, so a `false` here does not yet prove a color name is invalid.
#[must_use]
pub fn is_named_color(name: &str) -> bool {
    contains_ascii_ci(NAMED_COLORS_PARTIAL, name)
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
    fn partial_named_colors_recognized_case_insensitively() {
        assert!(is_named_color("red"));
        assert!(is_named_color("Red"));
        assert!(is_named_color("darkRed")); // legacy engine name
        assert!(is_named_color("lightGray"));
        assert!(is_named_color("transparent"));
        assert!(is_named_color("alpha"));
    }

    #[test]
    fn unknown_named_color_is_rejected() {
        assert!(!is_named_color("chartreuse-ish"));
        assert!(!is_named_color("notacolor"));
        assert!(!is_named_color(""));
    }
}
