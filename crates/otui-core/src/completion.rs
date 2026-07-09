//! Closed-set completion (spec §6): what fixed vocabulary applies at a byte offset.
//!
//! This module answers "the cursor is here — which of OTML's small **closed sets** should the
//! client offer?" It covers exactly the four sets the [`schema`](crate::schema) module owns:
//!
//! * `$state` selector names ([`schema::STATES`]),
//! * `anchors.<edge>` edges ([`schema::ANCHOR_EDGES`] + [`schema::SHORTHAND_ANCHORS`]),
//! * anchor **target** keywords ([`schema::MAGIC_ANCHOR_TARGETS`]) plus the in-scope widget `id:`
//!   values reachable in the current document, and target-side edges, and
//! * `@event` handler names ([`schema::EVENTS`]).
//!
//! **Deliberately out of scope** (returns an empty vec): property names and color names. Those come
//! from the large open-ish catalog the later `cargo xtask` extraction node produces, not from a
//! hand-transcribed list here (spec §6, §2.10). Property-value enums are likewise deferred. When no
//! closed set applies, this returns nothing — never a guess.
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

use crate::schema;
use crate::syntax::SyntaxTree;
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
}

/// Compute the completion candidates for the cursor at byte `offset` in `source` (spec §6).
///
/// Returns the matching closed set — `$state` names, anchor edges/targets, or `@event` names — or an
/// empty vec when the cursor is not in one of those contexts (a plain property value, a Lua body, an
/// out-of-range offset, …). The client is expected to filter the returned set by the partial word it
/// already has; we always return the whole set so the labels are deterministic (schema const order).
#[must_use]
pub fn complete_at(source: &str, offset: usize) -> Vec<CompletionItem> {
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
        Some(context) => items_for(context, source, offset),
        None => Vec::new(),
    }
}

/// Classify the closed-set context from the line `prefix` (line start up to the cursor).
///
/// Precise by construction: it only returns a context when the prefix unambiguously matches one of
/// the three grammar shapes AND the cursor is actively building the relevant token; anything else (a
/// property `key: value`, a completed token followed by whitespace, a tab-indented line, …) yields
/// `None`.
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

    None
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
        // A `key: value` line is not a closed-set context — property names/colors are the deferred
        // catalog node's job.
        let src = "Widget\n  color: red\n";
        assert!(complete_at(src, at(src, "red") + 1).is_empty());
        // ...and the property-name position offers nothing either (no hand-transcribed catalog).
        assert!(complete_at(src, at(src, "color") + 2).is_empty());
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
    fn states_context_is_deterministic() {
        // The returned labels for one context equal exactly the schema set, in const order.
        let src = "Button\n  $\n";
        assert_eq!(labels(&complete_at(src, at(src, "$") + 1)), schema::STATES);
    }
}
