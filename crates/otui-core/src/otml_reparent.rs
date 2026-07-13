//! Detects a line the engine parents onto an earlier **unique** sibling rather than genuinely
//! nesting under the block it visually sits inside — a tree-sitter grammar/engine mismatch that
//! otherwise makes a phantom `id:` or widget declaration look like an ordinary child of a
//! `container`/`style_header` walk.
//!
//! ## Why this exists
//!
//! `id_property`, `anchor_property` and `list_item` are the only three OTUI grammar rules
//! (`crates/tree-sitter-otui/grammar.js`) that end in `$._newline` with **no** `_block`
//! alternative — they can never carry an indented child in the grammar (every other rule that can
//! start a line offers `choice($._newline, $._block)`). When a line is written deeper-indented
//! under one of these three anyway, tree-sitter has nowhere to attach it *inside* that node, so it
//! reparents onto the enclosing block as an ordinary-looking next sibling, at a deeper column than
//! the line above it — verified directly against parse trees, not assumed (see this module's
//! tests).
//!
//! The real engine does something related but importantly different. `OTMLParser::parseLine`
//! reparents any line exactly one indent level deeper than the *immediately preceding* line onto
//! that line (`otmlparser.cpp:314`: `if (depth == currentDepth + 1) currentParent =
//! previousNode;`) — completely independent of which grammar rule that preceding line happens to
//! parse to. Whether the reparented child is ever turned into a widget then comes down to a single
//! test: `OTMLParser::parseNode` marks a node **unique** the instant its raw source line contains a
//! `:` (`otmlparser.cpp:435`: `node->setUnique(isUrlWithColon || dotsPos != std::string::npos)`,
//! with `dotsPos = line.find(':')` computed on the *whole* line, even for a `- item` list entry —
//! see below), and `UIManager::createWidgetFromOTML`'s child loop skips a unique node's children
//! outright (`uimanager.cpp:735`: `if (!childNode->isUnique()) createWidgetFromOTML(childNode,
//! widget);`).
//!
//! So the correct guard is **not** "is the preceding sibling one of the three block-less grammar
//! kinds" — that would be exactly the hand-copied-node-kind-list mistake this fix exists to
//! retire. It is: does the immediately preceding **named** sibling's own line contain a `:`, and
//! is this child indented strictly deeper than it. A `property` / `state_selector` /
//! `event_property` / `alias_property` / `expr_property` line always contains a `:` by
//! construction, so it is always engine-unique — but that case never reaches this check at all,
//! because those rules *do* have a `_block` alternative, so tree-sitter genuinely nests a
//! deeper-indented child **inside** them rather than reparenting it out as a sibling of the
//! enclosing block (also verified against the parse tree). A `container` / `style_header` line
//! never contains a `:` by construction (a colon would have made it parse as something else), so a
//! deeper child genuinely nested under one of *those* is a real, engine-created widget. A
//! `list_item`'s line (`- value`) usually has no colon either — so a deeper child under a plain
//! `- item` is real too — **except** when the item's own value text happens to contain a `:` (e.g.
//! `- key: value`), which the engine's `dotsPos = line.find(':')` still marks unique even though it
//! took the "`- item`" branch when splitting the line into tag/value. That corpus-legal edge case
//! is exactly why this stays a line-text test rather than a node-kind list.
//!
//! `id_property` itself can also be the reparented child (`id: a` followed by an over-indented
//! `id: b`), not just `container`/`style_header` — this module's check applies uniformly to every
//! child a caller is about to walk into, whatever kind it is.
//!
//! ## A false positive this guards against: a statement's own fields are not "previous siblings"
//!
//! `prev_named_sibling` finds the nearest *named* sibling, and tree-sitter fields (e.g.
//! `state_selector`'s own `state` name, `property`'s `key`/`value`) are named nodes too. A
//! genuinely nested block child's `prev_named_sibling` can therefore be one of *its own parent's*
//! header fields rather than a true preceding statement — `$pressed:`'s `state` field ("pressed")
//! sits immediately before a block child in `state_selector`'s children, and that field's own line
//! (`$pressed:`) does contain a `:`. Treating that as "the preceding line is unique" would wrongly
//! mark every block child of `state_selector`/`property`/etc. as reparented, even though those
//! rules *do* have a `_block` and the child is genuinely, correctly nested. So the check only fires
//! when the preceding sibling is itself one of the grammar's **statement** kinds (`$._node`'s
//! choices) — a true previous line the engine could have reparented onto — never a field of the
//! enclosing statement.

use tree_sitter::Node;

/// The grammar's `$._node` choices (`crates/tree-sitter-otui/grammar.js`) — every kind that is a
/// whole OTML *line*, as opposed to a field/token belonging to one.
const STATEMENT_KINDS: &[&str] = &[
    "style_header",
    "state_selector",
    "event_property",
    "alias_property",
    "expr_property",
    "anchor_property",
    "id_property",
    "list_item",
    "property",
    "container",
];

/// True when `child` is not really nested where the parse tree visually puts it: its immediately
/// preceding named sibling is itself a statement (see the module docs on why that restriction
/// matters) whose own line is engine-unique (contains a `:`, `otmlparser.cpp:435`), and `child`
/// starts at a strictly deeper column — so `OTMLParser::parseLine`'s reparenting rule
/// (`otmlparser.cpp:314`) actually attaches it under that unique sibling, where
/// `UIManager::createWidgetFromOTML` never creates it (`uimanager.cpp:735`). Callers should skip
/// `child` and its whole subtree: neither `child` itself nor anything nested inside it exists at
/// runtime.
#[must_use]
pub(crate) fn is_reparented_onto_a_unique_sibling(child: Node<'_>, source: &str) -> bool {
    // Skip `comment` nodes: they are tree-sitter `extras`, so they appear between real statements as
    // named siblings, but `OTMLParser::parseLine` discards a comment line before it updates
    // `previousNode`/`currentDepth` (`otmlparser.cpp`), so the statement the engine reparents onto is
    // the last *non-comment* line above `child`, not an interposed comment.
    let mut cursor = child.prev_named_sibling();
    while let Some(prev) = cursor {
        if prev.kind() == "comment" {
            cursor = prev.prev_named_sibling();
            continue;
        }
        return STATEMENT_KINDS.contains(&prev.kind())
            && child.start_position().column > prev.start_position().column
            && line_contains_colon(prev, source);
    }
    false
}

/// Whether `node`'s own source line — from its first byte (leading indentation is never part of a
/// node's span, so there is nothing to skip) to the next newline or end of source — contains a
/// `:`, mirroring `OTMLParser::parseNode`'s `dotsPos = line.find(':')` (`otmlparser.cpp:435`)
/// exactly, including its evaluation on the whole raw line rather than on `node`'s own
/// sub-fields (so a `- key: value` list item is correctly seen as unique).
fn line_contains_colon(node: Node<'_>, source: &str) -> bool {
    let start = node.start_byte();
    let end = source[start..]
        .find('\n')
        .map_or(source.len(), |i| start + i);
    source[start..end].contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::SyntaxTree;

    /// Parse `source` and return the named child at `path`, a sequence of named-child indices
    /// descended from the root (e.g. `&[0, 1]` = the root's first named child's second named
    /// child).
    fn node_at(source: &str, path: &[usize]) -> tree_sitter::Node<'static> {
        // Leak the tree so the returned Node's lifetime can outlive this helper — fine in tests.
        let tree: &'static SyntaxTree =
            Box::leak(Box::new(SyntaxTree::parse(source).expect("parse")));
        let mut node = tree.root();
        for &i in path {
            node = node.named_child(i).expect("child at path");
        }
        node
    }

    #[test]
    fn an_id_over_indented_under_another_id_is_reparented() {
        // Panel / id: a / id: b (over-indented) -- both id_property are direct children of the
        // Panel container. Path: document -> container "Panel" (0) -> named children
        // tag(0), id: a (1), id: b (2).
        let src = "Panel\n  id: a\n    id: b\n";
        let b = node_at(src, &[0, 2]);
        assert_eq!(b.kind(), "id_property");
        assert!(is_reparented_onto_a_unique_sibling(b, src));
    }

    #[test]
    fn an_id_over_indented_under_an_anchor_property_is_reparented() {
        let src = "Panel\n  anchors.left: parent.left\n    id: b\n";
        let b = node_at(src, &[0, 2]);
        assert_eq!(b.kind(), "id_property");
        assert!(is_reparented_onto_a_unique_sibling(b, src));
    }

    #[test]
    fn a_widget_over_indented_under_a_plain_id_is_reparented() {
        let src = "Panel\n  id: a\n    Button\n      id: c\n";
        let button = node_at(src, &[0, 2]);
        assert_eq!(button.kind(), "container");
        assert!(is_reparented_onto_a_unique_sibling(button, src));
    }

    #[test]
    fn a_style_header_over_indented_under_a_plain_id_is_reparented() {
        let src = "Panel\n  id: a\n    Button < Base\n      id: c\n";
        let header = node_at(src, &[0, 2]);
        assert_eq!(header.kind(), "style_header");
        assert!(is_reparented_onto_a_unique_sibling(header, src));
    }

    #[test]
    fn a_widget_over_indented_under_an_id_with_a_comment_between_is_reparented() {
        // A full-line comment is a tree-sitter `extra`, so it sits as a named sibling between the
        // unique `id: a` line and the over-indented `Child`. The engine discards the comment line
        // before advancing its parse state, so it reparents `Child` onto `id: a` and never creates
        // it — the guard must see past the comment to the real preceding statement.
        let src = "Panel\n  id: a\n  // note\n    Child\n      id: afterComment\n";
        let comment = node_at(src, &[0, 2]);
        assert_eq!(
            comment.kind(),
            "comment",
            "the `// note` line must parse as a comment"
        );
        let child = node_at(src, &[0, 3]);
        assert_eq!(child.kind(), "container");
        assert!(is_reparented_onto_a_unique_sibling(child, src));
    }

    #[test]
    fn a_widget_at_the_same_column_as_a_preceding_id_is_not_reparented() {
        // Ordinary sibling, same depth as `id: a` -- ordinary nesting, not a reparent.
        let src = "Panel\n  id: a\n  Button\n    id: normal\n";
        let button = node_at(src, &[0, 2]);
        assert_eq!(button.kind(), "container");
        assert!(!is_reparented_onto_a_unique_sibling(button, src));
    }

    #[test]
    fn the_first_child_of_a_block_has_no_preceding_sibling_and_is_never_reparented() {
        let src = "Panel\n  Button\n    id: x\n";
        let button = node_at(src, &[0, 1]);
        assert_eq!(button.kind(), "container");
        assert!(!is_reparented_onto_a_unique_sibling(button, src));
    }

    #[test]
    fn a_widget_over_indented_under_a_colon_less_list_item_is_a_real_engine_child() {
        // `- item` has no `:` anywhere on its line, so the engine's own `dotsPos` test marks it
        // NOT unique -- an over-indented child under it is genuinely created at runtime, and must
        // not be treated as reparented-away.
        let src = "Panel\n  - item\n    id: b\n";
        let id_b = node_at(src, &[0, 2]);
        assert_eq!(id_b.kind(), "id_property");
        assert!(!is_reparented_onto_a_unique_sibling(id_b, src));
    }

    #[test]
    fn a_list_item_whose_own_value_contains_a_colon_is_engine_unique() {
        // `- key: value`'s raw line DOES contain a `:` (inside the value text), so
        // `dotsPos = line.find(':')` still finds it and the engine marks this list item unique --
        // even though the "- item" branch is what actually splits its tag/value. A widget
        // over-indented under it is therefore never created.
        let src = "Panel\n  - key: value\n    Button\n      id: x\n";
        let button = node_at(src, &[0, 2]);
        assert_eq!(button.kind(), "container");
        assert!(is_reparented_onto_a_unique_sibling(button, src));
    }

    #[test]
    fn a_list_item_without_a_colon_lets_a_later_widget_nest_normally() {
        let src = "Panel\n  - item\n    Button\n      id: x\n";
        let button = node_at(src, &[0, 2]);
        assert_eq!(button.kind(), "container");
        assert!(!is_reparented_onto_a_unique_sibling(button, src));
    }

    #[test]
    fn a_genuinely_nested_style_header_under_a_state_block_is_not_reparented() {
        // Regression for a false positive: `state_selector`'s own `state` field ("pressed") is a
        // *named* node that sits immediately before the block's first statement, and its own line
        // (`$pressed:`) does contain a `:`. That field is not a preceding *statement* the engine
        // could have reparented onto, so it must not trip the guard -- `state_selector` genuinely
        // has a `_block` in the grammar, and `X < Base` here is really, correctly nested inside it.
        let src = "Outer < UIWidget\n  $pressed:\n    X < Base\n";
        // document -> style_header "Outer < UIWidget" (0) -> state_selector (2) -> style_header
        // "X < Base" (1), the block's first (and only) statement.
        let x = node_at(src, &[0, 2, 1]);
        assert_eq!(x.kind(), "style_header");
        assert!(!is_reparented_onto_a_unique_sibling(x, src));
    }

    #[test]
    fn a_genuinely_nested_id_under_a_plain_property_is_not_reparented() {
        // Same false-positive class, reached through `property`'s own `value` field instead of
        // `state_selector`'s `state` field.
        let src = "Panel\n  visible: false\n    id: phantom\n";
        // document -> container "Panel" (0) -> property "visible: false" (1) -> id_property (2),
        // the block's first (and only) statement.
        let id = node_at(src, &[0, 1, 2]);
        assert_eq!(id.kind(), "id_property");
        assert!(!is_reparented_onto_a_unique_sibling(id, src));
    }
}
