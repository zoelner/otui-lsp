//! Closed-set completion (spec §6): what fixed vocabulary applies at a byte offset.
//!
//! This module answers "the cursor is here — which fixed OTML vocabulary should the client offer?"
//! It covers the four closed sets the [`schema`](crate::schema) module owns:
//!
//! * `$state` selector names ([`schema::STATES`]),
//! * `anchors.<edge>` edges ([`schema::ANCHOR_EDGES`] + [`schema::SHORTHAND_ANCHORS`]),
//! * anchor **target** keywords ([`schema::MAGIC_ANCHOR_TARGETS`]) plus the in-scope widget `id:`
//!   values reachable in the current document, and target-side edges, and
//! * `@event` handler names ([`schema::EVENTS`]),
//!
//! plus one open-ish set from the generated catalog:
//!
//! * property **key** names ([`catalog::PROPERTIES`]) — offered while the cursor is building an
//!   ordinary `key:` on an indented line (spec §6, §2.10).
//!
//! **Deliberately out of scope** (returns an empty vec): property **values** — color names/forms and
//! per-property value enums. Color completion needs per-property color-type metadata the catalog does
//! not carry, so it is deferred until that metadata exists; likewise value enums. When no set applies,
//! this returns nothing — never a guess.
//!
//! ## How the context is classified
//!
//! Completion runs mid-edit, when the CST around the cursor is usually a half-typed `ERROR` region,
//! so a tree-shape match is unreliable here. Instead we classify from the **line prefix** — the text
//! from the line start up to the cursor — which is stable no matter how broken the rest of the line
//! is. The prefix rules mirror the grammar shapes (`crates/tree-sitter-otui/grammar.js`):
//!
//! * `state_selector` = `'$' repeat1(state) ':'` → a line whose first non-space char is `$` and
//!   which has no `:` yet is inside the selector → [`STATES`](crate::schema::STATES).
//! * `anchor_property` = `'anchors' '.' edge ':' anchor_target` → a line beginning `anchors.`:
//!   before the `:` we are on the **edge** key; after it, on the **target** value.
//! * `event_property` = `'@' event_name ':'` → a line whose first non-space char is `@` with no `:`
//!   yet is on the event **key** → [`EVENTS`](crate::schema::EVENTS). (After the `:` the value is
//!   embedded Lua — out of scope.)
//!
//! The one context the prefix alone cannot see is a multi-line `|` block-scalar body (raw Lua/text),
//! where a line may legitimately start with `$` / `@`. That is the only place we consult the CST: if
//! the cursor sits inside a `block_scalar_content` (or a `comment`), we suppress completion. The
//! grammar node kinds used are therefore `block_scalar_content` and `comment`.
//!
//! Everything here is pure: byte offsets in, [`CompletionItem`]s out. No I/O, no `lsp-types`.

use crate::catalog;
use crate::diagnostics;
use crate::lua_widgets::LuaWidgetIndex;
use crate::schema;
use crate::style_index::StyleIndex;
use crate::syntax::SyntaxTree;
use crate::widget_resolve;
use lang_api::{CompletionItem, CompletionKind};
use tree_sitter::Node;

/// The closed set that applies at the cursor, once the line prefix has been classified.
enum Context {
    /// Inside a `$state` selector: offer the 14 state names.
    States,
    /// On the property-side `anchors.<edge>` key: offer the 6 edges plus the 2 shorthands.
    AnchorEdgeKey,
    /// On the target-side edge after `<targetId>.` in an anchor value: offer the 6 edges.
    AnchorTargetEdge,
    /// At the start of an anchor value (target position): offer the magic target keywords.
    AnchorTarget,
    /// On an `@event` key: offer the known event handler names.
    Events,
    /// On an ordinary property **key** being typed (indented, no `:` yet): offer the catalog
    /// property names.
    PropertyKey,
}

/// Compute the completion candidates for the cursor at byte `offset` in `source` (spec §6).
///
/// Returns the matching set — `$state` names, anchor edges/targets, `@event` names, or (on an
/// indented bare `key`) the catalog property names — or an empty vec when the cursor is not in one of
/// those contexts (a property **value**, a Lua body, an out-of-range offset, …). The client is
/// expected to filter the returned set by the partial word it already has; we always return the whole
/// set so the labels are deterministic (schema/catalog const order).
#[must_use]
pub fn complete_at(source: &str, offset: usize) -> Vec<CompletionItem> {
    // The pure, workspace-unaware form: no style/Lua indexes, so a property key offers only the
    // global C++ catalog (the widget-added Lua properties come from the workspace-aware variant).
    complete_at_with_widgets(source, offset, &StyleIndex::new(), &LuaWidgetIndex::new())
}

/// Like [`complete_at`], but **widget-aware**: on an ordinary property key it additionally offers the
/// custom style properties the enclosing widget adds in Lua (e.g. `column-style` under a `UITable`),
/// resolved from the workspace `styles` + `lua` indexes. Every other context is identical. With empty
/// indexes it degrades exactly to [`complete_at`].
#[must_use]
pub fn complete_at_with_widgets(
    source: &str,
    offset: usize,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
) -> Vec<CompletionItem> {
    // Guard against an offset past the end or inside a multi-byte char: slicing the prefix below
    // would otherwise panic, and an offscreen cursor has no context anyway.
    if offset > source.len() || !source.is_char_boundary(offset) {
        return Vec::new();
    }

    // A `|` block-scalar body is raw Lua/text: a line inside it may start with `$`/`@` yet is not
    // OTML markup, so suppress there. This is the only place the CST is consulted (see module docs).
    if in_suppressed_context(source, offset) {
        return Vec::new();
    }

    let line_start = source[..offset].rfind('\n').map_or(0, |nl| nl + 1);
    let prefix = &source[line_start..offset];

    match classify(prefix) {
        // A property key additionally requires structurally valid indentation: an odd indent or a
        // depth jump of more than one level is a hard OTML parse error (the `diagnostics` module
        // flags `odd-indentation` / `invalid-indentation-depth`), so no key may be offered there.
        // This needs the whole document (the preceding lines' depth), so it is gated here rather
        // than in the line-prefix classifier. The closed-set contexts keep their own guards.
        Some(Context::PropertyKey) if !diagnostics::line_indentation_is_valid(source, offset) => {
            Vec::new()
        }
        Some(Context::PropertyKey) => property_key_items(source, offset, styles, lua),
        Some(context) => items_for(context, source, offset),
        None => Vec::new(),
    }
}

/// Build the property-key candidates: the global C++ catalog plus the Lua-added custom properties of
/// the widget enclosing the cursor (resolved cross-file through its `.otui` + Lua ancestry). The
/// catalog comes first (const order); the widget's Lua properties are appended, de-duplicated against
/// the catalog, and tagged so the client can tell them apart. When the enclosing widget cannot be
/// located (a parse too broken to find it) or has no Lua properties, this is just the catalog.
fn property_key_items(
    source: &str,
    offset: usize,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
) -> Vec<CompletionItem> {
    let mut items = set_items(catalog::PROPERTIES, CompletionKind::Keyword, "property");
    let Some(widget) = enclosing_widget_type(source, offset) else {
        return items;
    };
    let ancestry = widget_resolve::resolve_ancestry(&widget, styles, lua);
    for prop in ancestry.custom_properties(lua) {
        if items.iter().any(|item| item.label == prop) {
            continue; // a Lua property that shadows a catalog name collapses into one entry
        }
        items.push(CompletionItem {
            label: prop,
            kind: CompletionKind::Keyword,
            detail: Some("lua property".to_owned()),
        });
    }
    items
}

/// The type name of the widget enclosing the cursor: the `tag` of the nearest ancestor `container`
/// or the `base` of the nearest ancestor `style_header` (the type a property on that widget resolves
/// against, mirroring the diagnostics pass). `None` when the source cannot be parsed or the cursor
/// has no enclosing widget. Best-effort mid-edit: the current line is often a broken `ERROR` region,
/// but the enclosing widget itself is usually intact, so its type still resolves.
///
/// The half-typed key on the cursor's own line frequently parses as a bare `container` tag (a
/// lowercase word with no `:` yet is grammatically a widget tag), which is **not** the enclosing
/// widget — it is the property being typed. So any candidate widget that starts on the cursor's line
/// is skipped; only a widget declared on an earlier line is a genuine encloser.
fn enclosing_widget_type(source: &str, offset: usize) -> Option<String> {
    let tree = SyntaxTree::parse(source)?;
    let line_start = source[..offset].rfind('\n').map_or(0, |nl| nl + 1);
    let lo = offset.saturating_sub(1);
    let mut node = tree.root().descendant_for_byte_range(lo, offset)?;
    loop {
        // Skip a widget node sitting on the cursor's own line: it is the token being typed, not the
        // block that encloses it. A real enclosing widget opens on an earlier line.
        if node.start_byte() < line_start {
            match node.kind() {
                "container" => {
                    return node
                        .child_by_field_name("tag")
                        .map(|tag| source[tag.start_byte()..tag.end_byte()].to_owned());
                }
                "style_header" => {
                    return node
                        .child_by_field_name("base")
                        .map(|base| source[base.start_byte()..base.end_byte()].to_owned());
                }
                _ => {}
            }
        }
        node = node.parent()?;
    }
}

/// Classify the closed-set context from the line `prefix` (line start up to the cursor).
///
/// Precise by construction: it only returns a context when the prefix unambiguously matches one of
/// the grammar shapes (a `$state` / `anchors.` / `@event` special form, or a bare property `key`) AND
/// the cursor is actively building the relevant token; anything else (a property **value** after the
/// `:`, a completed token followed by whitespace, a tab-indented line, …) yields `None`.
fn classify(prefix: &str) -> Option<Context> {
    // Tab indentation is a hard OTML parse error (the engine rejects it — see the `tab-indentation`
    // diagnostic), so a tab-indented line is never valid markup: offer nothing on it.
    let indent = &prefix[..prefix.len() - prefix.trim_start().len()];
    if indent.contains('\t') {
        return None;
    }

    // Indentation is spaces; strip it to see the first meaningful char. (A leading `$`/`@`/`anchors`
    // is only ever at the token start of a line, after indentation.)
    let trimmed = prefix.trim_start();

    // `@event` key: the line opens with `@`, the cursor is still inside the single event-name token
    // (no whitespace yet), and it has not reached its `:` (past which the value is embedded Lua —
    // out of scope). A completed name followed by a space (`@onClick `) is no longer being built.
    if let Some(rest) = trimmed.strip_prefix('@') {
        let building = !rest.contains(':') && !rest.contains(char::is_whitespace);
        return building.then_some(Context::Events);
    }

    // `$state` selector: the line opens with `$` and has not reached its `:`. States are
    // space-separated, so offer while the cursor is building a state token — right after the `$`
    // (empty selector), or with a non-empty current segment (a partial state or a `!` negation) —
    // but not when it sits after a completed state followed by whitespace (`$hover `). A `$var` in
    // value position never reaches here — it sits after a `key:`, so the trimmed prefix starts with
    // the key, not `$`.
    if let Some(rest) = trimmed.strip_prefix('$') {
        if rest.contains(':') {
            return None;
        }
        let current = rest.rsplit(char::is_whitespace).next().unwrap_or("");
        let building = rest.is_empty() || !current.is_empty();
        return building.then_some(Context::States);
    }

    // `anchors.<edge>: <target>`: only the literal `anchors.` object counts (a generic dotted key
    // like `foo.left:` is not an anchor, per the grammar).
    if trimmed.starts_with("anchors.") {
        return match trimmed.find(':') {
            // Before the `:` → the edge key slot.
            None => Some(Context::AnchorEdgeKey),
            // After the `:` → the value slot.
            Some(colon) => classify_anchor_value(&trimmed[colon + 1..]),
        };
    }

    // An ordinary `key:` property — offer the catalog property names while the KEY is being typed.
    classify_property_key(indent, trimmed)
}

/// Classify the ordinary property **key** slot from the line's `indent` and its `trimmed` content.
///
/// Fires [`Context::PropertyKey`] only when the cursor is actively building a bare property-key
/// token, mirroring the `property_key` grammar shape (`token(IDENT)`, then letters/digits/`_`/`-`).
/// Precision over recall — anything ambiguous yields `None`:
///
/// * unindented (`indent` empty) → a top-level word is a widget tag / style header, never a property;
/// * empty word (just indentation, `  `) → ambiguous with a nested widget tag about to be typed;
/// * an **uppercase-initial** word → a child-widget tag / style header (`Button`, `UIWidget`), not a
///   property: OTML property names are all lowercase/kebab, so a leading uppercase letter marks a
///   widget position (widget-name completion is a separate future node);
/// * a `:` in the prefix → we are past the key, in **value** position (property-value/color
///   completion is deferred — see the module docs), so nothing;
/// * a `.` → a dotted key (`foo.bar`) is neither a property nor an `anchors.` object;
/// * whitespace, `<`, or any non-IDENT char → a completed word + trailing space, a `Name < Base`
///   header, or otherwise not a bare key.
///
/// The `$` / `@` / `anchors.` / `&` / `!` / `-` forms are already handled or excluded upstream (or by
/// the leading-char test), so they never reach here as a property key. Indentation-depth validity is
/// enforced separately by the caller (it needs the whole document).
fn classify_property_key(indent: &str, trimmed: &str) -> Option<Context> {
    // Properties are nested under a widget, so they always carry leading indentation. A top-level
    // (unindented) bare word is a widget/style header, not a property → offer nothing.
    if indent.is_empty() {
        return None;
    }
    // Require a non-empty word actively being built: an empty indented slot (`  `) is ambiguous with
    // a nested container tag about to be typed, so we stay silent (precision over recall).
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    // A property key starts with a LOWERCASE ASCII letter. OTML property names are all lowercase/kebab
    // (`width`, `image-source`, `text-align`), whereas a child-widget tag / style header at the same
    // indentation is CamelCase / uppercase-initial (`Button`, `Panel`, `UIWidget`) — the case
    // convention is what tells the two apart (every `catalog::PROPERTIES` entry is lowercase-initial).
    // A leading uppercase letter, digit, `_`, `&`, `!`, `-`, `.` or anything else is not a key here.
    if !first.is_ascii_lowercase() {
        return None;
    }
    // The rest must stay within the IDENT charset. A `:` (value position — deferred), `.` (dotted
    // key), `<` (style header), whitespace (completed word / multi-word tag), or any other char
    // means this is not a bare key still being typed.
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(Context::PropertyKey)
}

/// Classify the anchor **value** slot — the text after `anchors.<edge>:` up to the cursor.
///
/// The value is a single target token, so offer only while the cursor is building it:
/// * an empty value slot (just past the `:`) → the target start → [`Context::AnchorTarget`],
/// * a non-empty current segment containing `.` → a target-side edge → [`Context::AnchorTargetEdge`],
/// * a non-empty current segment without `.` → still the target position → [`Context::AnchorTarget`],
/// * a completed token followed by whitespace (`parent `) → nothing.
fn classify_anchor_value(value: &str) -> Option<Context> {
    let current = value.rsplit(char::is_whitespace).next().unwrap_or("");
    if current.is_empty() {
        // Empty slot (just after `:`) offers the target start; a trailing space after a completed
        // token does not.
        return value.trim().is_empty().then_some(Context::AnchorTarget);
    }
    if current.contains('.') {
        Some(Context::AnchorTargetEdge)
    } else {
        Some(Context::AnchorTarget)
    }
}

/// Build the [`CompletionItem`]s for a classified [`Context`], in a stable order (schema const order
/// for the closed sets; magic targets then in-scope ids for the anchor target slot).
fn items_for(context: Context, source: &str, offset: usize) -> Vec<CompletionItem> {
    match context {
        Context::States => set_items(schema::STATES, CompletionKind::EnumMember, "state"),
        Context::AnchorEdgeKey => {
            let mut items = set_items(
                schema::ANCHOR_EDGES,
                CompletionKind::EnumMember,
                "anchor edge",
            );
            items.extend(set_items(
                schema::SHORTHAND_ANCHORS,
                CompletionKind::EnumMember,
                "anchor shorthand",
            ));
            items
        }
        Context::AnchorTargetEdge => set_items(
            schema::ANCHOR_EDGES,
            CompletionKind::EnumMember,
            "anchor edge",
        ),
        Context::AnchorTarget => anchor_target_items(source, offset),
        Context::Events => set_items(schema::EVENTS, CompletionKind::Event, "event handler"),
        // Property KEY names from the generated catalog, in its (sorted) order. Property VALUES —
        // color names/forms and value enums — are deferred (no per-property type metadata yet), so
        // the value position deliberately falls through to an empty result upstream.
        Context::PropertyKey => set_items(catalog::PROPERTIES, CompletionKind::Keyword, "property"),
    }
}

/// Build the anchor **target** candidates: the magic pseudo-targets (`parent` / `next` / `prev`)
/// followed by the in-scope widget `id:` values reachable from the anchor's owning widget (spec §6).
///
/// Both carry [`CompletionKind::Value`]; the ids are tagged `"widget id"` to distinguish them in the
/// client. Magic targets come first (schema order), then ids in source order, de-duplicated by label
/// (an id literally named `parent` collapses into the magic entry).
fn anchor_target_items(source: &str, offset: usize) -> Vec<CompletionItem> {
    let mut items = set_items(
        schema::MAGIC_ANCHOR_TARGETS,
        CompletionKind::Value,
        "anchor target",
    );
    for id in scope_anchor_ids(source, offset) {
        if items.iter().any(|item| item.label == id) {
            continue;
        }
        items.push(CompletionItem {
            label: id,
            kind: CompletionKind::Value,
            detail: Some("widget id".to_owned()),
        });
    }
    items
}

/// Collect the widget `id:` values reachable as anchor targets from the widget owning the anchor at
/// `offset`: the owner's sibling widgets (same parent) plus its ancestor widgets, in source order,
/// de-duplicated. A purely **local** CST walk over the current document — anchor targets resolve
/// within one widget tree, never across files, so the cross-file style index is intentionally not
/// consulted. Returns empty on a parse failure or when the owner cannot be located.
fn scope_anchor_ids(source: &str, offset: usize) -> Vec<String> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let root = tree.root();
    let lo = offset.saturating_sub(1);
    let Some(mut node) = root.descendant_for_byte_range(lo, offset) else {
        return Vec::new();
    };
    // Walk up to the widget node that owns the anchor line the cursor sits on.
    let owner = loop {
        if is_widget(node) {
            break node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return Vec::new(),
        }
    };

    // In-scope widgets: the owner's sibling widgets (same parent, excluding the owner itself) and
    // its ancestor widgets.
    let mut scope: Vec<Node> = Vec::new();
    if let Some(parent) = owner.parent() {
        let mut cursor = parent.walk();
        for sibling in parent.named_children(&mut cursor) {
            if is_widget(sibling) && sibling.id() != owner.id() {
                scope.push(sibling);
            }
        }
    }
    let mut ancestor = owner.parent();
    while let Some(node) = ancestor {
        if is_widget(node) {
            scope.push(node);
        }
        ancestor = node.parent();
    }

    // Source order, de-duplicated by id text.
    scope.sort_by_key(Node::start_byte);
    let mut ids = Vec::new();
    for widget in scope {
        if let Some(id) = widget_id(widget, source) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Whether `node` is a widget node (a bare `container` tag or a `Name < Base` `style_header`).
fn is_widget(node: Node) -> bool {
    matches!(node.kind(), "container" | "style_header")
}

/// The `id:` value declared directly on `widget`, if any (its `id_property` child's value text,
/// trimmed). `None` when the widget declares no id.
fn widget_id(widget: Node, source: &str) -> Option<String> {
    let mut cursor = widget.walk();
    for child in widget.named_children(&mut cursor) {
        if child.kind() != "id_property" {
            continue;
        }
        let value = child.child_by_field_name("value")?;
        let text = source[value.start_byte()..value.end_byte()].trim();
        if !text.is_empty() {
            return Some(text.to_owned());
        }
    }
    None
}

/// Map a schema const slice to completion items with a shared `kind` and `detail`, preserving the
/// slice's order.
fn set_items(set: &[&str], kind: CompletionKind, detail: &str) -> Vec<CompletionItem> {
    set.iter()
        .map(|&label| CompletionItem {
            label: label.to_owned(),
            kind,
            detail: Some(detail.to_owned()),
        })
        .collect()
}

/// True if `offset` sits inside a raw region where OTML completion must not fire — a `|` block-scalar
/// body or a full-line comment. Best-effort: a parse failure (practically unreachable) is treated as
/// "not suppressed" so the prefix classifier still runs.
fn in_suppressed_context(source: &str, offset: usize) -> bool {
    let Some(tree) = SyntaxTree::parse(source) else {
        return false;
    };
    // Probe the byte just before the cursor (the char last typed) through the cursor, so a cursor at
    // the very end of a block-scalar line still lands inside its content node.
    let lo = offset.saturating_sub(1);
    let Some(mut node) = tree.root().descendant_for_byte_range(lo, offset) else {
        return false;
    };
    loop {
        if matches!(node.kind(), "block_scalar_content" | "comment") {
            return true;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// The labels of the returned items, in order — the determinism surface the tests assert on.
    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn cursor_after_dollar_offers_all_states() {
        let src = "Button\n  $\n";
        let offset = at(src, "$") + 1; // just past the `$`
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::STATES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn partial_state_word_still_offers_the_whole_set() {
        // The client filters by the partial word; the engine returns the full set deterministically.
        let src = "Button\n  $ho\n";
        let offset = at(src, "$ho") + 3;
        assert_eq!(labels(&complete_at(src, offset)), schema::STATES);
    }

    #[test]
    fn cursor_after_negation_in_a_multi_state_selector_offers_states() {
        let src = "Button\n  $hover !\n";
        let offset = at(src, "!") + 1;
        assert_eq!(labels(&complete_at(src, offset)), schema::STATES);
    }

    #[test]
    fn cursor_after_anchors_dot_offers_edges_and_shorthands() {
        let src = "Widget\n  anchors.\n";
        let offset = at(src, "anchors.") + "anchors.".len();
        let mut expected: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        expected.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(labels(&complete_at(src, offset)), expected);
        assert!(complete_at(src, offset)
            .iter()
            .all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn cursor_at_anchor_value_start_offers_magic_targets() {
        let src = "Widget\n  anchors.top: \n";
        let offset = at(src, "anchors.top: ") + "anchors.top: ".len();
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::MAGIC_ANCHOR_TARGETS);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
    }

    #[test]
    fn cursor_after_target_dot_in_anchor_value_offers_edges() {
        let src = "Widget\n  anchors.top: parent.\n";
        let offset = at(src, "parent.") + "parent.".len();
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::ANCHOR_EDGES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn cursor_after_at_offers_events() {
        let src = "Button\n  @\n";
        let offset = at(src, "@") + 1;
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::EVENTS);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Event));
    }

    #[test]
    fn partial_event_word_still_offers_the_whole_set() {
        let src = "Button\n  @onCl\n";
        let offset = at(src, "@onCl") + "@onCl".len();
        assert_eq!(labels(&complete_at(src, offset)), schema::EVENTS);
    }

    #[test]
    fn plain_property_value_offers_nothing() {
        // After the `:` we are in VALUE position — property-value/color completion is deferred (no
        // per-property color-type metadata), so nothing is offered there.
        let src = "Widget\n  color: red\n";
        assert!(complete_at(src, at(src, "red") + 1).is_empty());
        // The KEY position, by contrast, now offers the catalog property names (see the property-key
        // tests below): `  co` is a bare property key being typed.
        assert_eq!(
            labels(&complete_at(src, at(src, "color") + 2)),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn event_value_position_offers_nothing() {
        // After the `@event:` colon the value is embedded Lua — explicitly out of scope.
        let src = "Button\n  @onClick: foo\n";
        assert!(complete_at(src, at(src, "foo") + 1).is_empty());
    }

    #[test]
    fn generic_dotted_key_is_not_an_anchor() {
        // Only the literal `anchors.` object is an anchor; `foo.left:` is a generic property.
        let src = "Widget\n  foo.\n";
        assert!(complete_at(src, at(src, "foo.") + "foo.".len()).is_empty());
    }

    #[test]
    fn value_dollar_reference_is_not_a_state_selector() {
        // A `$var` in value position sits after `key:`, so the line does not open with `$`.
        let src = "Widget\n  color: $pri\n";
        assert!(complete_at(src, at(src, "$pri") + "$pri".len()).is_empty());
    }

    #[test]
    fn inside_a_block_scalar_body_offers_nothing() {
        // A `|` block body is raw Lua: a line starting with `$` there is not a state selector.
        let src = "Button\n  @onClick: |\n    $foo\n";
        let offset = at(src, "$foo") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn inside_a_full_line_comment_offers_nothing() {
        // A line-start `//` (or `#`) comment is prose, not markup: a `$`/`@` inside it must not
        // trigger a closed-set completion (the CST-suppression path for `comment`).
        let src = "Button\n  // $hover\n";
        let offset = at(src, "$hover") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn tab_indented_line_offers_nothing() {
        // Tab indentation is a hard OTML parse error; no closed set may be offered on such a line.
        let src = "Button\n\t$hover\n";
        let offset = at(src, "$") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn complete_event_name_followed_by_space_offers_nothing() {
        // `@onClick ` — the event name is complete and the cursor sits after a space (no `:` yet);
        // the token is no longer being built, so nothing is offered.
        let src = "Button\n  @onClick \n";
        let offset = at(src, "@onClick ") + "@onClick ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn complete_anchor_target_followed_by_space_offers_nothing() {
        // `anchors.top: parent ` — the target is complete and the cursor sits after a trailing
        // space; the target token is no longer being built, so nothing is offered.
        let src = "Widget\n  anchors.top: parent \n";
        let offset = at(src, "parent ") + "parent ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn still_building_tokens_keep_offering_their_sets() {
        // Partial tokens (the cursor actively building them) still get the whole set.
        let src = "Button\n  @onCl\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "@onCl") + "@onCl".len())),
            schema::EVENTS
        );

        let src = "Button\n  $hov\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "$hov") + "$hov".len())),
            schema::STATES
        );

        // `anchors.to` (partial edge, no colon) still offers edges + shorthands.
        let src = "Widget\n  anchors.to\n";
        let mut edges: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        edges.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(
            labels(&complete_at(
                src,
                at(src, "anchors.to") + "anchors.to".len()
            )),
            edges
        );

        // `anchors.top: par` (partial target) still offers the target set (magic targets here — no
        // ids in this doc).
        let src = "Widget\n  anchors.top: par\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "par\n") + "par".len())),
            schema::MAGIC_ANCHOR_TARGETS
        );
    }

    #[test]
    fn anchor_target_without_ids_offers_only_magic_targets() {
        // A document with no widget ids: the target slot offers just `parent`/`next`/`prev`.
        let src = "Widget\n  anchors.top: \n";
        let offset = at(src, "anchors.top: ") + "anchors.top: ".len();
        assert_eq!(
            labels(&complete_at(src, offset)),
            schema::MAGIC_ANCHOR_TARGETS
        );
    }

    #[test]
    fn anchor_target_includes_sibling_and_ancestor_ids() {
        // Button owns the anchor. Reachable ids: its sibling `Label` (id `lbl`) and its ancestor
        // `Panel` (id `root`); Button's own id `btn` is excluded (a widget cannot anchor to itself).
        let src = "\
Panel
  id: root
  Button
    id: btn
    anchors.top:
  Label
    id: lbl
";
        let offset = at(src, "anchors.top:") + "anchors.top:".len();
        let mut expected: Vec<String> = schema::MAGIC_ANCHOR_TARGETS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        // Source order: `root` (ancestor Panel, earlier in the file) then `lbl` (sibling Label).
        expected.push("root".to_owned());
        expected.push("lbl".to_owned());
        assert_eq!(labels(&complete_at(src, offset)), expected);
        // Button's own id is not a target; magic targets stay `Value`, ids are tagged `widget id`.
        let items = complete_at(src, offset);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
        assert!(!items.iter().any(|i| i.label == "btn"));
        assert_eq!(
            items
                .iter()
                .find(|i| i.label == "root")
                .and_then(|i| i.detail.as_deref()),
            Some("widget id")
        );
    }

    #[test]
    fn offscreen_or_unknown_offset_offers_nothing() {
        assert!(complete_at("", 0).is_empty());
        let src = "Button\n  $\n";
        assert!(complete_at(src, src.len() + 100).is_empty());
    }

    #[test]
    fn indented_partial_property_word_offers_the_catalog() {
        // `  wid` — a bare property key being typed on an indented line: offer all catalog
        // property names (the client filters by `wid` → `width`). Deterministic, catalog order.
        let src = "Button\n  wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        assert_eq!(labels(&items), catalog::PROPERTIES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Keyword));
        assert!(items
            .iter()
            .all(|i| i.detail.as_deref() == Some("property")));
    }

    #[test]
    fn property_key_context_is_deterministic() {
        // The returned labels equal exactly the catalog set, in const (sorted) order.
        let src = "Button\n  colo\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "colo") + "colo".len())),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn empty_indented_position_offers_nothing() {
        // DECISION (pinned): an empty indented slot (`  ` with no word yet) is ambiguous with a
        // nested container tag about to be typed, so we require a non-empty word — precision over
        // recall. Nothing is offered here.
        let src = "Button\n  \n";
        let offset = at(src, "  \n") + 2; // just past the two indent spaces
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn completed_property_key_and_colon_offers_nothing() {
        // `  width:` — past the `:` we are in VALUE position; property-value/color completion is
        // deferred (needs per-property type metadata), so nothing is offered.
        let src = "Button\n  width:\n";
        let offset = at(src, "width:") + "width:".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn completed_property_word_followed_by_space_offers_nothing() {
        // `  width ` — the key is complete and the cursor sits after a space (no `:` yet); the token
        // is no longer being built, so nothing is offered.
        let src = "Button\n  width \n";
        let offset = at(src, "width ") + "width ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn unindented_top_level_word_offers_nothing() {
        // `Button` at column 0 is a widget/style-header, NOT a property → offer nothing.
        let src = "Butt\n";
        assert!(complete_at(src, at(src, "Butt") + "Butt".len()).is_empty());
    }

    #[test]
    fn indented_uppercase_word_is_a_child_widget_not_a_property() {
        // An indented CamelCase word is a nested widget/tag position, not a property key. Property
        // names are all lowercase/kebab, so a leading uppercase letter marks a widget → offer
        // nothing (widget-name completion is a separate future node).
        let src = "Panel\n  Button\n";
        assert!(complete_at(src, at(src, "Button") + "Button".len()).is_empty());
        // ...but a lowercase-initial word at the very same indentation still offers the catalog.
        let src = "Panel\n  wid\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "wid") + "wid".len())),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn odd_indented_property_word_offers_nothing() {
        // A 3-space (odd) indent is a hard OTML error (`odd-indentation`); the key context must
        // honor the same indentation rule the diagnostics pass enforces → offer nothing.
        let src = "Panel\n   wid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn invalid_depth_jump_property_word_offers_nothing() {
        // `    wid` jumps from depth 0 (`Panel`) straight to depth 2 — an `invalid-indentation-depth`
        // error — so no property key may be offered on it.
        let src = "Panel\n    wid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn valid_plus_one_level_offers_the_catalog() {
        // A correctly-indented +1 level under a nested widget is valid depth: offer the catalog.
        let src = "Panel\n  Child\n    wid\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "wid") + "wid".len())),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn tab_indented_property_word_offers_nothing() {
        // Tab indentation is a hard OTML parse error; no set may be offered on such a line — the key
        // context honors the same guard as the closed sets.
        let src = "Button\n\twid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn dotted_key_is_not_a_property_key() {
        // A generic dotted key (`foo.bar`) is neither a property nor an `anchors.` object → nothing.
        let src = "Button\n  foo.bar\n";
        assert!(complete_at(src, at(src, "foo.bar") + "foo.bar".len()).is_empty());
    }

    #[test]
    fn list_item_line_is_not_a_property_key() {
        // A `- item` list line opens with `-`, which is not an IDENT start → nothing.
        let src = "Button\n  - item\n";
        assert!(complete_at(src, at(src, "- item") + "- item".len()).is_empty());
        // ...even mid-word right after the dash.
        assert!(complete_at(src, at(src, "- item") + "- ".len()).is_empty());
    }

    #[test]
    fn special_form_keys_still_offer_their_own_sets_not_properties() {
        // The property-key branch runs AFTER the special forms, so `$` / `@` / `anchors.` keep
        // offering their own closed sets and never fall through to PROPERTIES.
        let src = "Button\n  $\n";
        assert_eq!(labels(&complete_at(src, at(src, "$") + 1)), schema::STATES);

        let src = "Button\n  @onC\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "@onC") + "@onC".len())),
            schema::EVENTS
        );

        let src = "Widget\n  anchors.\n";
        let mut edges: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        edges.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(
            labels(&complete_at(src, at(src, "anchors.") + "anchors.".len())),
            edges
        );
    }

    #[test]
    fn property_key_suppressed_inside_block_scalar_and_comment() {
        // A bare word inside a `|` block body is raw Lua, and one inside a full-line comment is
        // prose — neither is a property key. The CST-suppression guard covers the key branch too.
        let src = "Button\n  @onClick: |\n    width\n";
        assert!(complete_at(src, at(src, "width\n") + "width".len()).is_empty());

        let src = "Button\n  // width\n";
        assert!(complete_at(src, at(src, "width\n") + "width".len()).is_empty());
    }

    #[test]
    fn states_context_is_deterministic() {
        // The returned labels for one context equal exactly the schema set, in const order.
        let src = "Button\n  $\n";
        assert_eq!(labels(&complete_at(src, at(src, "$") + 1)), schema::STATES);
    }

    // --- widget-aware property completion (Lua-added properties) -------------------------------

    use crate::lua_widgets::scan_widgets;
    use crate::style_index::extract_style_defs;

    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(*doc, extract_style_defs(&tree));
        }
        index
    }

    fn lua(docs: &[(&str, &str)]) -> LuaWidgetIndex {
        let mut index = LuaWidgetIndex::new();
        for (doc, src) in docs {
            index.set_document(*doc, scan_widgets(src));
        }
        index
    }

    const UITABLE_LUA: &str = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    elseif name == 'row-style' then
    end
  end
end
";

    #[test]
    fn property_key_offers_lua_props_of_the_enclosing_widget() {
        // Under a `< UITable` header, the property key context offers the catalog PLUS UITable's
        // Lua-added properties, deterministically (catalog first, then the Lua props sorted).
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Table < UITable\n  col\n";
        let items = complete_at_with_widgets(src, at(src, "col") + "col".len(), &styles, &lua);
        let got = labels(&items);

        let mut expected: Vec<String> = catalog::PROPERTIES
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        expected.push("column-style".to_owned());
        expected.push("row-style".to_owned());
        assert_eq!(got, expected);
        // The Lua props are tagged so the client can distinguish them from catalog properties.
        let col = items.iter().find(|i| i.label == "column-style").unwrap();
        assert_eq!(col.detail.as_deref(), Some("lua property"));
        assert_eq!(col.kind, CompletionKind::Keyword);
    }

    #[test]
    fn property_key_lua_props_resolve_cross_file_on_a_widget_instance() {
        // The cursor is under a nested `Table` container whose type resolves cross-file
        // (`Table < UITable`) to the native UITable that declares the Lua props.
        let styles = styles(&[("lib.otui", "Table < UITable\n")]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "\
Window < UIWindow
  Table
    col
";
        let items = complete_at_with_widgets(src, at(src, "col") + "col".len(), &styles, &lua);
        let got = labels(&items);
        assert!(got.contains(&"column-style".to_owned()));
        assert!(got.contains(&"row-style".to_owned()));
    }

    #[test]
    fn property_key_omits_lua_props_on_an_unrelated_widget() {
        // A Button does not descend from UITable, so its property completion is the catalog only.
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Button < UIButton\n  col\n";
        let got = labels(&complete_at_with_widgets(
            src,
            at(src, "col") + "col".len(),
            &styles,
            &lua,
        ));
        assert_eq!(got, catalog::PROPERTIES);
    }

    #[test]
    fn empty_indexes_degrade_to_the_catalog_only() {
        // The workspace-unaware `complete_at` (empty indexes) offers exactly the catalog.
        let src = "Table < UITable\n  col\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "col") + "col".len())),
            catalog::PROPERTIES
        );
    }
}
