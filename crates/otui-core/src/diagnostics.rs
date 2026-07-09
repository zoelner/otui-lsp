//! Parse-level diagnostics (spec §4), a faithful mirror of the OTClient OTML parser.
//!
//! This is the *parse* category only — every finding here is a [`Severity::Error`], because these
//! are conditions the real engine treats as fatal (`OTMLException`) or that leave the tree-sitter
//! grammar unable to form a valid node. Higher-level, engine-tolerated authoring mistakes
//! (unknown properties, unknown `$state`, style-resolution warnings) are *hints/warnings* handled
//! by later milestones and are intentionally **not** produced here.
//!
//! Three passes contribute:
//!
//! 1. A **line-based indentation** pass mirroring `OTMLParser::getLineDepth` / `parseLine`:
//!    tabs in leading whitespace, odd (non-multiple-of-2) indentation, and invalid depth jumps.
//! 2. A **structural** pass harvesting tree-sitter `ERROR` / `MISSING` nodes for malformed
//!    constructs (e.g. an unterminated inline array).
//! 3. A **semantic** pass over the CST that consumes the closed sets in [`crate::schema`] to flag
//!    the two spec §4 checks those sets enable: an unknown `$state` selector name
//!    ([`UNKNOWN_STATE`], a *hint* — the engine never errors on it, it just never matches) and an
//!    invalid anchor edge ([`INVALID_ANCHOR_EDGE`], an *error* — `anchors.*` is one of the four
//!    value-validating properties of spec §2.10), and an unknown ordinary property name
//!    ([`UNKNOWN_PROPERTY`], a *hint* — the engine silently applies-or-ignores a tag it does not
//!    recognize, so a misspelled/unknown property never errors, it just has no effect).

use crate::schema;
use crate::syntax::SyntaxTree;
use lang_api::{ByteSpan, Diagnostic, Severity};
use tree_sitter::Node;

/// Diagnostic code: a tab appears in a structural line's leading indentation.
pub const TAB_INDENTATION: &str = "tab-indentation";
/// Diagnostic code: leading spaces are not a multiple of two.
pub const ODD_INDENTATION: &str = "odd-indentation";
/// Diagnostic code: a line's depth exceeds the previous line's depth by more than one level.
pub const INVALID_INDENTATION_DEPTH: &str = "invalid-indentation-depth";
/// Diagnostic code: a structural (`ERROR`/`MISSING`) parse node.
pub const SYNTAX_ERROR: &str = "syntax-error";
/// Diagnostic code: a `$state` selector name outside the closed 14-name set (spec §2.8). Severity
/// [`Severity::Hint`]: the engine's `translateState` returns `InvalidState`, so the block simply
/// never matches — a probable authoring bug, not an engine error.
pub const UNKNOWN_STATE: &str = "unknown-state";
/// Diagnostic code: an `anchors.<edge>` key edge or a `<target>.<edge>` value edge that is not one
/// of the six anchor edges (spec §2.4). Severity [`Severity::Error`]: `anchors.*` is one of the
/// four *value-validating* properties (spec §2.10), so the engine throws on a bad edge.
pub const INVALID_ANCHOR_EDGE: &str = "invalid-anchor-edge";
/// Diagnostic code: an ordinary property key that is not a known OTML property name (spec §2.10,
/// §4). Severity [`Severity::Hint`]: the engine silently applies-or-ignores an unrecognized tag
/// (`node->tag()` matches nothing), so a misspelled/unknown property is never an error or warning —
/// only a gentle hint that the property will have no effect. Value validation (border/display/
/// layout/color) is a separate concern; this check is the unknown-KEY hint only.
pub const UNKNOWN_PROPERTY: &str = "unknown-property";

/// Computes all parse-level diagnostics for `source`.
///
/// Returns findings sorted by span (`start`, then `end`). The document is parsed once; the two
/// passes share nothing beyond the source text.
#[must_use]
pub fn analyze(source: &str) -> Vec<Diagnostic> {
    let mut out = indentation_pass(source);
    // The tree is parsed once and shared by the structural and semantic passes.
    if let Some(tree) = SyntaxTree::parse(source) {
        collect_structural_errors(tree.root(), &mut out);
        collect_semantic_diagnostics(tree.root(), source, &mut out);
    }
    out.sort_by_key(|d| (d.span.start, d.span.end));
    out
}

/// One physical line of the source, sliced without its trailing `\n` (a trailing `\r` is kept and
/// treated as ordinary trailing whitespace, matching the engine's right-trim).
struct Line<'a> {
    /// Byte offset of the line's first character within the source.
    start: usize,
    /// The line text, excluding the terminating `\n`.
    text: &'a str,
}

/// Splits `source` into lines carrying their byte offsets.
fn split_lines(source: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            lines.push(Line {
                start,
                text: &source[start..i],
            });
            start = i + 1;
        }
    }
    if start < source.len() {
        lines.push(Line {
            start,
            text: &source[start..],
        });
    }
    lines
}

/// Number of leading ASCII space (`' '`) bytes — tabs and other bytes stop the count, exactly like
/// `getLineDepth`'s `while (line[spaces] == ' ')`.
fn leading_spaces(text: &str) -> usize {
    text.bytes().take_while(|&b| b == b' ').count()
}

/// The value portion of a structural line, used only to detect block-scalar markers so their raw
/// content lines can be skipped by the indentation pass. Mirrors the tag/value split of
/// `parseNode` closely enough for that purpose (list items via a leading `-`, otherwise the text
/// after the first `:`).
fn line_value(trimmed: &str) -> &str {
    if let Some(rest) = trimmed.strip_prefix('-') {
        return rest.trim();
    }
    match trimmed.find(':') {
        Some(pos) => trimmed[pos + 1..].trim(),
        None => "",
    }
}

fn is_block_scalar_marker(value: &str) -> bool {
    matches!(value, "|" | "|-" | "|+")
}

fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with('#')
}

/// The line-based indentation validation pass (`getLineDepth` + `parseLine`).
fn indentation_pass(source: &str) -> Vec<Diagnostic> {
    let lines = split_lines(source);
    let mut out = Vec::new();
    let mut current_depth: usize = 0;
    let mut i = 0;

    while i < lines.len() {
        let line = &lines[i];
        i += 1;

        let trimmed = line.text.trim();
        // Blank lines: `getLineDepth` returns 0 and `parseLine` skips them — no checks, no effect
        // on depth.
        if trimmed.is_empty() {
            continue;
        }
        // Comment lines are skipped by `parseLine` and do not affect structural depth.
        if is_comment(trimmed) {
            continue;
        }

        let sp = leading_spaces(line.text);
        let bytes = line.text.as_bytes();
        let mut indent_flagged = false;

        // The engine checks for a tab first (`line[spaces] == '\t'`) and, only if absent, for odd
        // indentation. Preserve that precedence so a single malformed line yields one finding.
        if bytes.get(sp) == Some(&b'\t') {
            out.push(Diagnostic {
                severity: Severity::Error,
                code: TAB_INDENTATION,
                message: "indentation with tabs is not allowed".to_owned(),
                span: ByteSpan::new(line.start + sp, line.start + sp + 1),
            });
            indent_flagged = true;
        } else if sp % 2 != 0 {
            out.push(Diagnostic {
                severity: Severity::Error,
                code: ODD_INDENTATION,
                message: "indentation must be a multiple of 2 spaces".to_owned(),
                span: ByteSpan::new(line.start, line.start + sp),
            });
            indent_flagged = true;
        }

        let depth = sp / 2;

        // `parseLine`: a jump of more than one level (`depth > currentDepth + 1`) is fatal. Skip
        // this check when the line already has an indentation error to avoid double-flagging.
        if !indent_flagged && depth > current_depth + 1 {
            out.push(Diagnostic {
                severity: Severity::Error,
                code: INVALID_INDENTATION_DEPTH,
                message: "invalid indentation depth".to_owned(),
                span: ByteSpan::new(line.start + sp, line.start + line.text.trim_end().len()),
            });
        }

        current_depth = depth;

        // Block scalars (`|`, `|-`, `|+`): the engine consumes deeper-indented lines as raw text
        // (`getLineDepth(line, /*multilining=*/true)` skips tab/odd checks for them). Skip those
        // content lines here so their indentation is not validated as structure.
        if is_block_scalar_marker(line_value(trimmed)) {
            while i < lines.len() {
                let content = &lines[i];
                if content.text.trim().is_empty() {
                    // Blank lines inside/after the block are consumed and keep the block open.
                    i += 1;
                    continue;
                }
                // Compare raw leading-space counts against the opening property line's `sp`, not
                // halved depths: halving loses parity, so a content line indented by an odd number
                // of extra spaces (e.g. `sp + 1`) could wrongly compute the same depth as the
                // marker line and be treated as structure. Content is anything deeper than `sp`.
                let content_sp = leading_spaces(content.text);
                if content_sp > sp {
                    i += 1; // raw block content
                } else {
                    break; // next structural node — reprocess in the outer loop
                }
            }
        }
    }

    out
}

/// Depth-first harvest of `ERROR` and `MISSING` nodes. An `ERROR` node's subtree is not descended
/// into (the whole malformed region is reported once); `MISSING` nodes are reported wherever they
/// appear.
fn collect_structural_errors(node: Node<'_>, out: &mut Vec<Diagnostic>) {
    if node.is_error() {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: SYNTAX_ERROR,
            message: "syntax error".to_owned(),
            span: SyntaxTree::span_of(node),
        });
        return;
    }
    if node.is_missing() {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: SYNTAX_ERROR,
            message: format!("missing {}", node.kind()),
            span: SyntaxTree::span_of(node),
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_structural_errors(child, out);
    }
}

/// Depth-first semantic validation over the CST. Recurses through every node so that anchors and
/// `$state` blocks are validated wherever they appear (anchors are per-widget and both may sit on
/// arbitrarily nested widgets). Ordinary `property` nodes are validated the same way, since a
/// property may appear on any widget at any depth. Only these node kinds carry a finding; every
/// other kind is transparent and simply recursed through.
fn collect_semantic_diagnostics(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    match node.kind() {
        "state_selector" => check_state_selector(node, source, out),
        "anchor_property" => check_anchor_property(node, source, out),
        "property" => check_property(node, source, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_semantic_diagnostics(child, source, out);
    }
}

/// Slice `source` by a node's byte span.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

/// Validate each state name in a `$state[ !state...]:` selector (grammar: `state_selector` holds one
/// or more `state` children, each with a `name` field aliased to `state_name` and an optional
/// `!`-negation). A name outside the closed 14-name set is a [`Severity::Hint`]; the span is the
/// name token only, so a `$valid !bogus:` selector yields exactly one hint on `bogus`. Membership is
/// case-insensitive (per `schema::is_known_state`), so a mis-cased known state produces nothing.
fn check_state_selector(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "state" {
            continue;
        }
        let Some(name) = child.child_by_field_name("name") else {
            continue;
        };
        if !schema::is_known_state(slice(source, name)) {
            out.push(Diagnostic {
                severity: Severity::Hint,
                code: UNKNOWN_STATE,
                message: format!(
                    "unknown state `{}`: never matches at runtime",
                    slice(source, name)
                ),
                span: SyntaxTree::span_of(name),
            });
        }
    }
}

/// Validate an `anchors.<edge>: <target>` node (grammar: `anchor_property` with an `edge` field
/// aliased to `anchor_edge` and an optional `value` field of kind `anchor_target` whose `target`
/// field is a dotted `identifier`). Two edge tokens are checked; the target *id* is intentionally
/// not resolved here (cross-file id existence is a later node).
fn check_anchor_property(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    // Property-side edge: must be one of the six edges OR a shorthand key (`fill`/`centerIn`, which
    // the grammar spells as the edge of `anchors.fill:` / `anchors.centerIn:`).
    if let Some(edge) = node.child_by_field_name("edge") {
        let text = slice(source, edge);
        if !schema::is_anchor_edge(text) && !schema::is_shorthand_anchor(text) {
            out.push(Diagnostic {
                severity: Severity::Error,
                code: INVALID_ANCHOR_EDGE,
                message: format!("`{text}` is not a valid anchor edge"),
                span: SyntaxTree::span_of(edge),
            });
        }
    }

    // Target-side edge: in `<targetId>.<targetEdge>` the suffix after the last `.` must be an edge.
    // A dot-less value (`parent`, `next`, `prev`, `none`, or the shorthand `anchors.fill: parent`
    // form) carries no target edge and is intentionally left unvalidated here — we do not resolve
    // the target id. A trailing-dot / empty suffix is also skipped (prefer a false negative to a
    // false positive on a shape the grammar does not cleanly delimit).
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    let Some(target) = value.child_by_field_name("target") else {
        return;
    };
    let text = slice(source, target);
    let Some(dot) = text.rfind('.') else {
        return;
    };
    let edge = &text[dot + 1..];
    if edge.is_empty() || schema::is_anchor_edge(edge) {
        return;
    }
    let start = target.start_byte() + dot + 1;
    out.push(Diagnostic {
        severity: Severity::Error,
        code: INVALID_ANCHOR_EDGE,
        message: format!("`{edge}` is not a valid anchor edge"),
        span: ByteSpan::new(start, target.end_byte()),
    });
}

/// Validate an ordinary `key: value` property (grammar: `property` with a `key` field of kind
/// `property_key`). A key that is not a known OTML property name is a [`Severity::Hint`]; the span
/// is the key token only.
///
/// Only the plain `property` kind reaches here. The grammar routes the non-catalog forms to their
/// own node kinds, which are therefore *not* matched by this function and never flagged as an
/// unknown property: `anchors.<edge>` (`anchor_property`), `@event`/`&alias`/`!expr` Lua-bearing tags
/// (`event_property`/`alias_property`/`expr_property`), `id:` (`id_property`), `$state` selectors
/// (`state_selector`), list items (`list_item`), and a nested widget / style header
/// (`container`/`style_header`). Property VALUES are intentionally not validated here (border/
/// display/layout/color value validation is a separate node); this is the unknown-KEY hint only.
///
/// Membership is [`schema::is_known_property`], an exact case-sensitive compare (the engine
/// dispatches on `node->tag() == "..."`), so a mis-cased `Width` is unknown → hint. That is faithful.
///
/// A `property` that carries a nested block (`key:` with indented children) is *not* flagged: the
/// grammar spells a colon-keyed group as a `property` with a `_block`, but such a node acts as a
/// container/subtree (a child widget group, or a `key:`/`- item` list parent), not a leaf style
/// property. Per the "prefer false negatives over false positives" rule, we only flag leaf
/// properties (a bare `key:` or `key: value` with no nested statements).
fn check_property(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    let Some(key) = node.child_by_field_name("key") else {
        return;
    };
    // Detect a nested block: any *named* child that is neither the `key` field nor the `value` field
    // is a statement inside a `_block`, so the node is a container form — skip it.
    let value = node.child_by_field_name("value");
    let mut cursor = node.walk();
    let has_block = node
        .named_children(&mut cursor)
        .any(|child| child.id() != key.id() && value.is_none_or(|v| child.id() != v.id()));
    if has_block {
        return;
    }
    let name = slice(source, key);
    if !schema::is_known_property(name) {
        out.push(Diagnostic {
            severity: Severity::Hint,
            code: UNKNOWN_PROPERTY,
            message: format!("unknown property `{name}`: applied-or-ignored, has no effect"),
            span: SyntaxTree::span_of(key),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code).collect()
    }

    #[test]
    fn clean_document_has_no_diagnostics() {
        let src = "\
MainWindow < UIWindow
  id: main
  size: 100 200
  Button
    id: ok
    @onClick: |
      self:hide()
";
        assert!(analyze(src).is_empty(), "clean doc: {:?}", analyze(src));
    }

    #[test]
    fn tab_indentation_is_flagged_at_the_tab() {
        // Second line is indented with a tab.
        let src = "Panel\n\tid: main\n";
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![TAB_INDENTATION]);
        let d = &diags[0];
        assert!(d.severity == Severity::Error);
        // The tab is the first byte of line 2 (after "Panel\n" = 6 bytes).
        assert_eq!(d.span, ByteSpan::new(6, 7));
        assert_eq!(&src[d.span.start..d.span.end], "\t");
    }

    #[test]
    fn tab_after_spaces_is_flagged() {
        let src = "Panel\n  \tid: x\n"; // two spaces then a tab
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![TAB_INDENTATION]);
        // "Panel\n" = 6, plus two spaces => tab at byte 8.
        assert_eq!(diags[0].span, ByteSpan::new(8, 9));
    }

    #[test]
    fn odd_one_space_indentation_is_flagged() {
        let src = "Panel\n id: x\n"; // one space
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![ODD_INDENTATION]);
        assert_eq!(diags[0].span, ByteSpan::new(6, 7));
    }

    #[test]
    fn odd_three_space_indentation_is_flagged() {
        let src = "Panel\n  id: x\n   size: y\n"; // 0,2,3 spaces
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![ODD_INDENTATION]);
    }

    #[test]
    fn invalid_depth_jump_is_flagged() {
        // 0 -> 2 levels (4 spaces) with no intervening level.
        let src = "Panel\n    id: x\n";
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![INVALID_INDENTATION_DEPTH]);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn deep_but_valid_nesting_is_not_flagged() {
        // `a`/`b`/`c` carry nested blocks (container form, not leaf style properties) so they are
        // not unknown-property candidates; `width` is a known leaf property. The test exercises
        // depth-4 nesting producing no indentation-depth diagnostics.
        let src = "\
a:
  b:
    c:
      width: 1
";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn comments_and_blanks_do_not_affect_depth() {
        let src = "\
Panel

  // a comment
  id: main

  size: 1 2
";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn block_scalar_content_is_not_validated_as_structure() {
        // The lua body is indented far past one level and would otherwise look like a depth jump
        // and odd indentation; inside a block scalar it must be ignored.
        let src = "\
btn:
  @onClick: |
       self:hide()
  id: y
";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn block_scalar_content_with_odd_extra_indent_is_not_flagged() {
        // The property line `@onClick: |` sits at 2 leading spaces (current_depth = 1). A content
        // line at 3 spaces is only one space deeper: halving both (3 / 2 = 1) used to make the
        // content look like it was at the *same* depth as the marker, so the old check wrongly
        // broke out of the block and reprocessed this line as structure, flagging it
        // `odd-indentation`. It must be treated as raw block-scalar content instead.
        let src = "\
Panel
  @onClick: |
   one()
  id: y
";
        let diags = analyze(src);
        assert!(diags.is_empty(), "{:?}", diags);
    }

    #[test]
    fn block_scalar_content_with_five_space_indent_is_not_flagged() {
        let src = "\
Panel
  @onClick: |-
     one()
  id: y
";
        let diags = analyze(src);
        assert!(diags.is_empty(), "{:?}", diags);
    }

    #[test]
    fn malformed_inline_array_is_a_syntax_error() {
        let src = "x: [a, b\n";
        let diags = analyze(src);
        assert!(
            codes(&diags).contains(&SYNTAX_ERROR),
            "expected syntax-error, got {:?}",
            diags
        );
        let d = diags.iter().find(|d| d.code == SYNTAX_ERROR).unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(!d.span.is_empty());
    }

    #[test]
    fn tab_takes_precedence_over_odd_on_the_same_line() {
        // One space then a tab: engine checks the tab first and never reaches the odd check.
        let src = "Panel\n \tid: x\n";
        let diags = analyze(src);
        assert_eq!(codes(&diags), vec![TAB_INDENTATION]);
    }

    #[test]
    fn diagnostics_are_sorted_by_span() {
        let src = "Panel\n id: a\n   size: b\n";
        let diags = analyze(src);
        for w in diags.windows(2) {
            assert!(w[0].span.start <= w[1].span.start);
        }
    }

    // --- semantic pass: unknown $state (hint) -------------------------------------------------

    fn only<'a>(diags: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
        let matches: Vec<_> = diags.iter().filter(|d| d.code == code).collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one {code}, got {diags:?}"
        );
        matches[0]
    }

    #[test]
    fn unknown_state_is_a_hint_on_the_name_token() {
        let src = "\
Button
  $foo:
    color: red
";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_STATE);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "foo");
    }

    #[test]
    fn known_states_produce_no_hint_including_miscased() {
        // Every known state, plus a deliberately mis-cased one, must be silent (case-insensitive).
        for state in [
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
            "hidden",
            "mobile",
            "Hover",
            "PRESSED",
        ] {
            let src = format!("Button\n  ${state}:\n    color: red\n");
            let diags = analyze(&src);
            assert!(
                diags.iter().all(|d| d.code != UNKNOWN_STATE),
                "state `{state}` must not be flagged: {diags:?}"
            );
        }
    }

    #[test]
    fn state_selector_with_valid_and_invalid_yields_one_hint() {
        // `$hover !bogus:` — one valid, one unknown (negated) — exactly one hint, on `bogus`.
        let src = "\
Button
  $hover !bogus:
    color: red
";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_STATE);
        assert_eq!(&src[d.span.start..d.span.end], "bogus");
    }

    // --- semantic pass: invalid anchor edge (error) -------------------------------------------

    #[test]
    fn invalid_property_side_anchor_edge_is_an_error() {
        let src = "\
Widget
  anchors.middle: parent.top
";
        let diags = analyze(src);
        let d = only(&diags, INVALID_ANCHOR_EDGE);
        assert_eq!(d.severity, Severity::Error);
        // Span is the property-side edge token `middle`, not the valid target edge `top`.
        assert_eq!(&src[d.span.start..d.span.end], "middle");
    }

    #[test]
    fn invalid_target_side_anchor_edge_is_an_error() {
        let src = "\
Widget
  anchors.top: parent.bogus
";
        let diags = analyze(src);
        let d = only(&diags, INVALID_ANCHOR_EDGE);
        assert_eq!(d.severity, Severity::Error);
        // Only the offending target-edge token is spanned, not the whole `parent.bogus`.
        assert_eq!(&src[d.span.start..d.span.end], "bogus");
    }

    #[test]
    fn valid_anchors_with_magic_targets_and_shorthands_are_silent() {
        let src = "\
Widget
  anchors.top: parent.top
  anchors.left: sibling.right
  anchors.fill: parent
  anchors.centerIn: parent
  anchors.bottom: none
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_ANCHOR_EDGE),
            "valid anchors must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn anchor_edge_check_is_case_insensitive() {
        // The engine lowercases the token before matching, so a mis-cased edge is still valid.
        let src = "\
Widget
  anchors.HorizontalCenter: parent.VerticalCenter
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_ANCHOR_EDGE),
            "case-insensitive edges must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn parse_level_and_semantic_diagnostics_coexist() {
        // A tab-indented line (parse level) alongside an unknown state (semantic) — both fire.
        let src = "Panel\n\tid: main\n$foo:\n  color: red\n";
        let diags = analyze(src);
        let cs = codes(&diags);
        assert!(
            cs.contains(&TAB_INDENTATION),
            "parse level intact: {diags:?}"
        );
        assert!(cs.contains(&UNKNOWN_STATE), "semantic present: {diags:?}");
    }

    // --- semantic pass: unknown property (hint) -----------------------------------------------

    #[test]
    fn known_property_produces_no_hint() {
        let src = "\
Panel
  width: 10
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_PROPERTY),
            "known property must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn unknown_property_is_a_hint_on_the_key_token() {
        // `widht` is a typo for `width`.
        let src = "\
Panel
  widht: 10
";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "widht");
    }

    #[test]
    fn miscased_known_property_is_a_hint() {
        // `is_known_property` is an exact byte compare (`node->tag() == "width"`), so `Width` is
        // unknown at runtime — a faithful hint.
        let src = "\
Panel
  Width: 10
";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "Width");
    }

    #[test]
    fn non_property_kinds_are_never_flagged_as_unknown_property() {
        // id:, anchors.top:, @onClick:, a $state block, a list item, and a nested widget header are
        // all their own grammar nodes — none is an ordinary catalog `property`, so none is flagged.
        let src = "\
Panel
  id: main
  anchors.top: parent.top
  @onClick: |
    self:hide()
  $hover:
    color: red
  items:
    - one
    - two
  Button
    id: ok
  Label < UILabel
    id: title
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_PROPERTY),
            "no unknown-property should fire here: {diags:?}"
        );
    }

    #[test]
    fn unknown_property_nested_in_child_widget_is_flagged() {
        // A property may sit on a widget at any depth; the child's typo must still be flagged.
        let src = "\
Panel
  id: main
  Button
    id: ok
    widht: 10
";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "widht");
    }

    #[test]
    fn unknown_property_never_error_or_warning() {
        // Hard fidelity rule: an unknown property is a hint, never an error or warning.
        let src = "Panel\n  not-a-real-prop: 1\n";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_ne!(d.severity, Severity::Error);
        assert_ne!(d.severity, Severity::Warning);
        assert_eq!(d.severity, Severity::Hint);
    }
}
