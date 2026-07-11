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

/// The file-path-valued OTUI property tags: property keys whose *value* the engine treats as a
/// filesystem path to a texture/image and loads via `g_textures.getTexture`. Verified against the
/// engine's `UIWidget` style parsing: `image-source` is passed to `setImageSource` (which calls
/// `g_textures.getTexture`) in `parseImageStyle`, and the `icon` / `icon-source` tags are resolved
/// as a path and passed to `setIcon` (also `g_textures.getTexture`) in `parseBaseStyle`. This is the
/// exhaustive set of genuinely path-valued tags; it is deliberately small and precise (properties
/// that are merely image-*related* — offsets, rects, colors — are NOT here). It can be extended if a
/// fork introduces another path-valued tag.
pub const PATH_PROPERTIES: &[&str] = &["image-source", "icon", "icon-source"];

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

/// A resolved color as four channels, each a normalized `f32` in `[0, 1]` — the shape the LSP
/// `documentColor` feature wants (LSP `Color`). Computed by [`color_value`] from any of the OTML
/// color forms (hex / functional / named), faithful to the engine's `Color::operator>>`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    /// Build an [`Rgba`] from integer `0..=255` channels (the engine's native `uint8_t` channels),
    /// normalizing each to `[0, 1]`.
    #[must_use]
    pub(crate) fn from_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        }
    }
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

/// True if `name` is a named color the engine recognizes: a CSS-table color
/// ([`crate::catalog::NAMED_COLORS`]), a legacy engine color static
/// ([`crate::catalog::LEGACY_COLORS`]), or the `transparent` alias
/// ([`crate::catalog::LEGACY_COLOR_NAMES`]). Case-insensitive, matching the engine's `css_lookup`,
/// which lowercases the token before comparing. Every table is machine-extracted from the engine's
/// color source, so a `false` here genuinely means the name is not an engine-recognized color.
#[must_use]
pub fn is_named_color(name: &str) -> bool {
    let needle = name.trim();
    crate::catalog::NAMED_COLORS
        .iter()
        .chain(crate::catalog::LEGACY_COLORS)
        .any(|(n, _)| n.eq_ignore_ascii_case(needle))
        || contains_ascii_ci(crate::catalog::LEGACY_COLOR_NAMES, needle)
}

/// The packed `0xRRGGBB` value of a CSS named color ([`crate::catalog::NAMED_COLORS`]), or `None`
/// when `name` is not a valued CSS color. Case-insensitive, matching the engine's `css_lookup`.
///
/// This is the CSS-table lookup only; the engine's legacy color statics
/// ([`crate::catalog::LEGACY_COLORS`], which carry alpha) and the `transparent` alias are resolved
/// by [`color_value`] directly. A name absent from the CSS table returns `None` here even if it is a
/// legacy color.
#[must_use]
pub fn named_color_value(name: &str) -> Option<u32> {
    let needle = name.trim();
    crate::catalog::NAMED_COLORS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(needle))
        .map(|(_, value)| *value)
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

/// True if `name` is a key the engine reads **inside a `layout:` block**
/// ([`crate::catalog::LAYOUT_PROPERTIES`]).
///
/// These keys (`type`, `spacing`, `fit-children`, `cell-size`, `num-columns`, `flow`, …) are
/// dispatched by the *layout object* (`UIBoxLayout` / `UIGridLayout` / … `::applyStyle`), not by the
/// widget style parser, so they are absent from [`crate::catalog::PROPERTIES`] and are **only** valid
/// nested under a `layout:` block — at widget level they are genuinely unknown. Callers must
/// therefore gate on the block context; see `diagnostics::check_property`.
///
/// **Exact match**, like [`is_known_property`]: the engine dispatches on `node->tag() == "..."`.
///
/// The catalog holds the **union** across the layout classes. The engine instantiates one layout per
/// widget and each class ignores the keys it does not read, so a key belonging to a different layout
/// type (`spacing:` under `type: grid`, say) is silently ignored rather than an error — accepting the
/// union therefore prefers a false negative over a false positive, as the diagnostics rule requires.
#[must_use]
pub fn is_layout_block_property(name: &str) -> bool {
    crate::catalog::LAYOUT_PROPERTIES.contains(&name)
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

/// Resolve a color value string to its actual [`Rgba`] channels, faithful to the engine's
/// `Color::operator>>` (`src/framework/util/color.cpp`). Returns `None` for anything that is not a
/// valid color.
///
/// Handles every OTML color form (spec §2.9):
/// * **hex** `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa` — 3/4-digit bodies are nibble-doubled,
///   alpha defaults to opaque.
/// * **functional** `rgb()` / `rgba()` (byte-or-percent channels) and `hsl()` / `hsla()`
///   (converted via the engine's `hsl_to_rgb`), reusing the same validation as
///   [`is_valid_color`].
/// * **named** — the `transparent` alias is fully transparent; a legacy engine static
///   ([`crate::catalog::LEGACY_COLORS`], alpha preserved) is tried next (the engine checks its
///   statics before the CSS table, so e.g. `green` is the engine's bright `0x00ff00`); any other
///   name resolves through the CSS table ([`named_color_value`]).
#[must_use]
pub fn color_value(value: &str) -> Option<Rgba> {
    let v = value.trim();

    if let Some(body) = v.strip_prefix('#') {
        return hex_value(body);
    }
    if is_valid_color(v) {
        // A valid non-hex color is functional (hex is handled above).
        return functional_value(v);
    }
    // Named colors. `transparent` is fully transparent (alpha 0), distinct from any RGB entry.
    if v.eq_ignore_ascii_case("transparent") {
        return Some(Rgba::from_u8(0, 0, 0, 0));
    }
    // Legacy engine statics first (engine precedence), unpacking the alpha-carrying 0xRRGGBBAA.
    if let Some((_, rgba)) = crate::catalog::LEGACY_COLORS
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(v))
    {
        let [r, g, b, a] = unpack_rgba(*rgba);
        return Some(Rgba::from_u8(r, g, b, a));
    }
    named_color_value(v).map(|rgb| {
        let [r, g, b] = unpack_rgb(rgb);
        Rgba::from_u8(r, g, b, 255)
    })
}

/// The textual color forms to offer for `colorPresentation` (spec §2.9), given a picked color: the
/// canonical hex (`#rrggbb`, or `#rrggbbaa` when the color is not fully opaque) first, then the
/// functional `rgb(r, g, b)` / `rgba(r, g, b, a)` spelling. Every string is a form the engine's
/// parser accepts, so applying it round-trips. Channels are rounded to `0..=255`.
#[must_use]
pub fn color_presentations(color: Rgba) -> Vec<String> {
    let r = to_u8(color.r);
    let g = to_u8(color.g);
    let b = to_u8(color.b);
    let a = to_u8(color.a);
    if a == 255 {
        vec![
            format!("#{r:02x}{g:02x}{b:02x}"),
            format!("rgb({r}, {g}, {b})"),
        ]
    } else {
        vec![
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}"),
            format!("rgba({r}, {g}, {b}, {a})"),
        ]
    }
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

/// Unpack a packed `0xRRGGBB` into `[r, g, b]` bytes.
fn unpack_rgb(rgb: u32) -> [u8; 3] {
    [
        ((rgb >> 16) & 0xFF) as u8,
        ((rgb >> 8) & 0xFF) as u8,
        (rgb & 0xFF) as u8,
    ]
}

/// Unpack a packed `0xRRGGBBAA` (the legacy-color layout) into `[r, g, b, a]` bytes.
fn unpack_rgba(rgba: u32) -> [u8; 4] {
    [
        ((rgba >> 24) & 0xFF) as u8,
        ((rgba >> 16) & 0xFF) as u8,
        ((rgba >> 8) & 0xFF) as u8,
        (rgba & 0xFF) as u8,
    ]
}

/// Round a normalized `[0, 1]` channel to a `0..=255` byte, clamping out-of-range inputs.
fn to_u8(channel: f32) -> u8 {
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Compute the [`Rgba`] of a hex color body (the text after `#`), faithful to the engine: a length-3
/// or -4 body is nibble-doubled to 6/8; a length-6 or -8 body is used as-is; every char must be a
/// hex digit; a 6-digit body is opaque. Any other length or a non-hex char yields `None`.
fn hex_value(body: &str) -> Option<Rgba> {
    if !is_valid_hex_body(body) {
        return None;
    }
    let expanded: String = if matches!(body.len(), 3 | 4) {
        body.chars().flat_map(|c| [c, c]).collect()
    } else {
        body.to_owned()
    };
    let byte = |i: usize| u8::from_str_radix(&expanded[i..i + 2], 16).ok();
    let r = byte(0)?;
    let g = byte(2)?;
    let b = byte(4)?;
    let a = if expanded.len() == 8 { byte(6)? } else { 255 };
    Some(Rgba::from_u8(r, g, b, a))
}

/// Compute the [`Rgba`] of a functional `rgb()/rgba()/hsl()/hsla()` color, faithful to the engine's
/// `operator>>` (whitespace stripped, comma-split, `parse_byte_or_percent` for `rgb` channels,
/// `hsl_to_rgb` for `hsl`, `parse_alpha_any` for the alpha channel). Returns `None` if the value is
/// not a well-formed functional color (mirrors [`is_valid_functional`]).
fn functional_value(value: &str) -> Option<Rgba> {
    let stripped: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let (prefix, is_hsl, has_alpha) = ["rgba", "hsla", "rgb", "hsl"]
        .into_iter()
        .find(|name| stripped.starts_with(&format!("{name}(")))
        .map(|name| (name, name.starts_with("hsl"), name.ends_with('a')))?;

    let inner = stripped
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
        .filter(|s| !s.is_empty())?;

    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != if has_alpha { 4 } else { 3 } {
        return None;
    }
    if !parts.iter().all(|p| is_numeric_component(p)) {
        return None;
    }

    let alpha = if has_alpha {
        parse_alpha_any(parts[3])
    } else {
        255
    };
    let (r, g, b) = if is_hsl {
        let h = strtod(parts[0]);
        let s = hsl_percent(parts[1]);
        let l = hsl_percent(parts[2]);
        hsl_to_rgb(h, s, l)
    } else {
        (
            parse_byte_or_percent(parts[0]),
            parse_byte_or_percent(parts[1]),
            parse_byte_or_percent(parts[2]),
        )
    };
    Some(Rgba::from_u8(r, g, b, alpha))
}

/// Clamp an `i32` to a `0..=255` byte (the engine's `clamp255`).
fn clamp255(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Parse the leading decimal of a functional-color token as `strtod` does (reads the numeric prefix,
/// tolerating a trailing `%` or other suffix). The token is already validated numeric, so a bare
/// numeric prefix always parses; anything unparsable yields `0.0`.
fn strtod(s: &str) -> f64 {
    let end = s
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E') || *i == 0))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse::<f64>().unwrap_or(0.0)
}

/// Parse the leading integer of a token as `std::stoi` does (leading sign + digits, stopping at the
/// first non-digit — so `stoi("1.5") == 1`). Yields `0` when there is no leading integer.
fn stoi(s: &str) -> i32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let digits_end = bytes[i..]
        .iter()
        .position(|b| !b.is_ascii_digit())
        .map_or(bytes.len(), |p| i + p);
    if digits_end == i {
        return 0;
    }
    s[..digits_end].parse::<i32>().unwrap_or(0)
}

/// An `rgb()` channel: a trailing `%` scales `p% -> p*255/100`; otherwise an integer byte. Both are
/// clamped to `0..=255`. Mirrors the engine's `parse_byte_or_percent`.
fn parse_byte_or_percent(s: &str) -> u8 {
    if s.ends_with('%') {
        clamp255((strtod(s) * 255.0 / 100.0).round() as i32)
    } else {
        clamp255(stoi(s))
    }
}

/// An alpha channel: a trailing `%` is a percentage; a value carrying `.`, `e` or `E` is a `[0, 1]`
/// float scaled to a byte; otherwise an integer byte. Mirrors the engine's `parse_alpha_any`.
fn parse_alpha_any(s: &str) -> u8 {
    if s.ends_with('%') {
        return clamp255((strtod(s) * 255.0 / 100.0).round() as i32);
    }
    if s.contains(['.', 'e', 'E']) {
        let f = strtod(s).clamp(0.0, 1.0);
        return clamp255((f * 255.0).round() as i32);
    }
    clamp255(stoi(s))
}

/// An `hsl()` saturation/lightness component: a trailing `%` scales to `[0, 1]`; otherwise the raw
/// number is clamped to `[0, 1]`. Mirrors the engine's `pct` lambda in `operator>>`.
fn hsl_percent(s: &str) -> f64 {
    let v = strtod(s);
    if s.ends_with('%') {
        (v / 100.0).clamp(0.0, 1.0)
    } else {
        v.clamp(0.0, 1.0)
    }
}

/// Convert HSL (`h` in degrees, `s`/`l` in `[0, 1]`) to `0..=255` RGB, faithful to the engine's
/// `hsl_to_rgb`.
fn hsl_to_rgb(mut h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let hue2rgb = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            return p + (q - p) * 6.0 * t;
        }
        if t < 1.0 / 2.0 {
            return q;
        }
        if t < 2.0 / 3.0 {
            return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
        }
        p
    };
    let (rf, gf, bf) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let hk = h / 360.0;
        (
            hue2rgb(p, q, hk + 1.0 / 3.0),
            hue2rgb(p, q, hk),
            hue2rgb(p, q, hk - 1.0 / 3.0),
        )
    };
    (
        clamp255((rf * 255.0).round() as i32),
        clamp255((gf * 255.0).round() as i32),
        clamp255((bf * 255.0).round() as i32),
    )
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

    /// Assert two [`Rgba`] are equal within a small epsilon (rounding tolerance).
    fn assert_rgba(actual: Rgba, r: u8, g: u8, b: u8, a: u8) {
        let expected = Rgba::from_u8(r, g, b, a);
        let close = |x: f32, y: f32| (x - y).abs() < 1e-4;
        assert!(
            close(actual.r, expected.r)
                && close(actual.g, expected.g)
                && close(actual.b, expected.b)
                && close(actual.a, expected.a),
            "{actual:?} != rgba({r},{g},{b},{a})"
        );
    }

    #[test]
    fn hex_color_values_of_each_length() {
        // 3-digit body is nibble-doubled (#abc -> #aabbcc); alpha defaults opaque.
        assert_rgba(color_value("#abc").unwrap(), 0xaa, 0xbb, 0xcc, 255);
        // 4-digit -> #aabbccdd (with alpha).
        assert_rgba(color_value("#abcd").unwrap(), 0xaa, 0xbb, 0xcc, 0xdd);
        // 6-digit used as-is, opaque.
        assert_rgba(color_value("#ff0000").unwrap(), 255, 0, 0, 255);
        // 8-digit carries alpha.
        assert_rgba(color_value("#11223344").unwrap(), 0x11, 0x22, 0x33, 0x44);
        // Uppercase hex digits are fine.
        assert_rgba(color_value("#FF00FF").unwrap(), 255, 0, 255, 255);
    }

    #[test]
    fn functional_rgb_values_including_percent() {
        assert_rgba(color_value("rgb(255, 0, 0)").unwrap(), 255, 0, 0, 255);
        // rgba alpha as a 0-255 byte.
        assert_rgba(color_value("rgba(255, 0, 0, 128)").unwrap(), 255, 0, 0, 128);
        // rgba alpha as a [0,1] float scales to a byte.
        assert_rgba(color_value("rgba(0, 0, 0, 0.5)").unwrap(), 0, 0, 0, 128);
        // Percent channels are valid for rgb too (engine fidelity): 50% -> 128.
        assert_rgba(
            color_value("rgb(50%, 50%, 50%)").unwrap(),
            128,
            128,
            128,
            255,
        );
        // Whitespace is stripped before parsing.
        assert_rgba(color_value("rgb( 10 , 20 , 30 )").unwrap(), 10, 20, 30, 255);
    }

    #[test]
    fn functional_hsl_values_convert_to_rgb() {
        // hsl(120, 100%, 50%) is pure green.
        assert_rgba(color_value("hsl(120, 100%, 50%)").unwrap(), 0, 255, 0, 255);
        // hsl(0, 0%, 0%) is black; saturation 0 => gray of the lightness.
        assert_rgba(color_value("hsl(0, 0%, 100%)").unwrap(), 255, 255, 255, 255);
        // hsla carries alpha.
        assert_rgba(
            color_value("hsla(240, 100%, 50%, 0.5)").unwrap(),
            0,
            0,
            255,
            128,
        );
    }

    #[test]
    fn named_color_values_from_catalog_case_insensitive() {
        // `red` is both a legacy static and a CSS name; legacy wins (engine precedence) — same RGB.
        assert_rgba(color_value("red").unwrap(), 255, 0, 0, 255);
        assert_rgba(color_value("RED").unwrap(), 255, 0, 0, 255);
        // A CSS-only color resolves to its packed RGB (opaque), case-insensitively.
        assert_rgba(color_value("aliceblue").unwrap(), 0xF0, 0xF8, 0xFF, 255);
        // `transparent` is fully transparent.
        assert_rgba(color_value("transparent").unwrap(), 0, 0, 0, 0);
        // Legacy engine statics now resolve (alpha preserved). `darkPink` is legacy-only; `alpha` is
        // the fully-transparent legacy static.
        assert!(is_named_color("darkPink"));
        assert_rgba(color_value("darkPink").unwrap(), 0x80, 0x00, 0x80, 255);
        assert_rgba(color_value("alpha").unwrap(), 0, 0, 0, 0);
        // Engine precedence: legacy `green` is bright 0x00ff00, NOT the darker CSS `green` (0x008000).
        assert_rgba(color_value("green").unwrap(), 0, 255, 0, 255);
    }

    #[test]
    fn named_color_value_lookup() {
        // `named_color_value` is the CSS-table lookup only (packed 0xRRGGBB), independent of the
        // legacy statics.
        assert_eq!(named_color_value("red"), Some(0xFF0000));
        assert_eq!(named_color_value("AliceBlue"), Some(0xF0F8FF));
        assert_eq!(named_color_value("darkPink"), None); // legacy-only, not in CSS table
        assert_eq!(named_color_value("notacolor"), None);
    }

    #[test]
    fn non_color_values_yield_no_rgba() {
        assert!(color_value("").is_none());
        assert!(color_value("Hello World").is_none());
        assert!(color_value("10").is_none());
        assert!(color_value("#ab").is_none()); // bad hex length
        assert!(color_value("rgb(1, 2)").is_none()); // too few args
        assert!(color_value("notacolor").is_none());
    }

    #[test]
    fn color_presentations_offer_hex_and_functional() {
        // Opaque -> #rrggbb + rgb(...).
        let opaque = Rgba::from_u8(255, 128, 0, 255);
        assert_eq!(
            color_presentations(opaque),
            vec!["#ff8000".to_owned(), "rgb(255, 128, 0)".to_owned()]
        );
        // Translucent -> #rrggbbaa + rgba(...).
        let translucent = Rgba::from_u8(255, 128, 0, 128);
        assert_eq!(
            color_presentations(translucent),
            vec![
                "#ff8000".to_owned() + "80",
                "rgba(255, 128, 0, 128)".to_owned()
            ]
        );
    }

    #[test]
    fn color_presentation_strings_round_trip_through_color_value() {
        // Every offered form must parse back to (approximately) the same color.
        let c = Rgba::from_u8(12, 200, 250, 128);
        for form in color_presentations(c) {
            let back = color_value(&form).unwrap_or_else(|| panic!("`{form}` should parse"));
            assert_rgba(back, 12, 200, 250, 128);
        }
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
        // The color-typed property set is non-empty and contains the main `color` tag plus a
        // `border-color*` family member (the value-validation work confirmed these are color-parsed).
        assert!(!crate::catalog::COLOR_PROPERTIES.is_empty());
        assert!(crate::catalog::COLOR_PROPERTIES.contains(&"color"));
        assert!(crate::catalog::COLOR_PROPERTIES.contains(&"border-color"));
        // Every color-typed tag is also a known property.
        for tag in crate::catalog::COLOR_PROPERTIES {
            assert!(
                is_known_property(tag),
                "color property `{tag}` should be in PROPERTIES"
            );
        }
        // The legacy color statics were extracted with values (alpha-carrying).
        assert!(
            crate::catalog::LEGACY_COLORS.len() >= 15,
            "expected the legacy engine color statics, got {}",
            crate::catalog::LEGACY_COLORS.len()
        );
    }

    #[test]
    fn path_properties_are_the_texture_path_tags_and_known_properties() {
        // The file-path-valued set: primarily `image-source`, plus the `icon` family — verified
        // against the engine's `setImageSource` / `setIcon` (both `g_textures.getTexture`) sites.
        assert!(PATH_PROPERTIES.contains(&"image-source"));
        assert!(PATH_PROPERTIES.contains(&"icon"));
        assert!(PATH_PROPERTIES.contains(&"icon-source"));
        // Every path-valued tag is also a known property (dispatched by the engine's style parsers).
        for tag in PATH_PROPERTIES {
            assert!(
                is_known_property(tag),
                "path property `{tag}` should be in PROPERTIES"
            );
        }
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
