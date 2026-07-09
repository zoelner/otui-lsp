//! Closed-set completion (spec §6): what fixed vocabulary applies at a byte offset.
//!
//! This module answers "the cursor is here — which of OTML's small **closed sets** should the
//! client offer?" It covers exactly the four sets the [`schema`](crate::schema) module owns:
//!
//! * `$state` selector names ([`schema::STATES`]),
//! * `anchors.<edge>` edges ([`schema::ANCHOR_EDGES`] + [`schema::SHORTHAND_ANCHORS`]),
//! * anchor **target** keywords ([`schema::MAGIC_ANCHOR_TARGETS`]) and target-side edges, and
//! * `@event` handler names ([`schema::EVENTS`]).
//!
//! **Deliberately out of scope** (returns an empty vec): property names and color names. Those come
//! from the large open-ish catalog the later `cargo xtask` extraction node produces, not from a
//! hand-transcribed list here (spec §6, §2.10). Property-value enums and sibling-id anchor targets
//! are likewise deferred. When no closed set applies, this returns nothing — never a guess.
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
        Some(context) => items_for(context),
        None => Vec::new(),
    }
}

/// Classify the closed-set context from the line `prefix` (line start up to the cursor).
///
/// Precise by construction: it only returns a context when the prefix unambiguously matches one of
/// the three grammar shapes; anything else (a property `key: value`, a bare tag, an inheritance
/// header, …) yields `None`.
fn classify(prefix: &str) -> Option<Context> {
    // Indentation is spaces; strip it to see the first meaningful char. (A leading `$`/`@`/`anchors`
    // is only ever at the token start of a line, after indentation.)
    let trimmed = prefix.trim_start();

    // `@event` key: the line opens with `@` and has not reached its `:` yet. Past the `:` the value
    // is embedded Lua (out of scope), so a `:` in the prefix disqualifies it.
    if let Some(rest) = trimmed.strip_prefix('@') {
        return (!rest.contains(':')).then_some(Context::Events);
    }

    // `$state` selector: the line opens with `$` and has not reached its `:` yet. A `$var` in value
    // position never reaches here — it sits after a `key:`, so the trimmed prefix starts with the
    // key, not `$`.
    if let Some(rest) = trimmed.strip_prefix('$') {
        return (!rest.contains(':')).then_some(Context::States);
    }

    // `anchors.<edge>: <target>`: only the literal `anchors.` object counts (a generic dotted key
    // like `foo.left:` is not an anchor, per the grammar).
    if trimmed.starts_with("anchors.") {
        return Some(match trimmed.find(':') {
            // Before the `:` → the edge key slot.
            None => Context::AnchorEdgeKey,
            // After the `:` → the value slot. A target-side edge (after `<targetId>.`) vs. the bare
            // target position is decided by whether the word being typed already has a `.`.
            Some(colon) => {
                let value_word = trimmed[colon + 1..]
                    .rsplit(|c: char| c.is_whitespace())
                    .next()
                    .unwrap_or("");
                if value_word.contains('.') {
                    Context::AnchorTargetEdge
                } else {
                    Context::AnchorTarget
                }
            }
        });
    }

    None
}

/// Build the [`CompletionItem`]s for a classified [`Context`], in schema const order (stable).
fn items_for(context: Context) -> Vec<CompletionItem> {
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
        Context::AnchorTarget => set_items(
            schema::MAGIC_ANCHOR_TARGETS,
            CompletionKind::Value,
            "anchor target",
        ),
        Context::Events => set_items(schema::EVENTS, CompletionKind::Event, "event handler"),
    }
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
