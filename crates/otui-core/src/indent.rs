//! Line-based indentation primitives, and [`indent_for_line`]: the on-type indentation the server's
//! `textDocument/onTypeFormatting` provider needs the instant a user presses Enter.
//!
//! ## Why this cannot reuse [`crate::format`]
//!
//! [`format::format`](crate::format::format) and
//! [`format::format_line_edits`](crate::format::format_line_edits) carry a hard safety gate: if
//! [`SyntaxTree::parse`](crate::syntax::SyntaxTree::parse) fails, or the tree contains any `ERROR` /
//! `MISSING` node, they return `None` and make no edit. That gate is correct for reformatting a
//! whole document. But on-type formatting fires **exactly when the document is mid-edit and
//! broken** — the instant after Enter, the new line is empty and the surrounding tree is very likely
//! sitting in an `ERROR` region. The formatter would return `None` precisely when it is needed, so
//! its CST-ancestor depth computation (`format::collect`, `format::LineMeta`) is unusable here.
//!
//! [`indent_for_line`] instead builds on the same **lexical** approach
//! [`crate::diagnostics::line_indentation_is_valid`] already uses for the same reason: it mirrors
//! `OTMLParser::getLineDepth` / `parseLine` purely from line text, never from the CST, so it keeps
//! working on a document that does not parse. This module is the crate's shared home for those line
//! primitives ([`Line`], [`split_lines`], [`leading_spaces`], plus the block-scalar/comment helpers);
//! [`crate::diagnostics`] consumes them from here rather than keeping its own copy.
//!
//! ## The rule
//!
//! The target indentation for line `line` is computed from the nearest **preceding**, non-blank,
//! non-comment line:
//!
//! * if that line **opens a block** — a style header (`Name < Base`), a bare container tag (an
//!   identifier with no `:`), a `$state` selector (`$hover:`), or any colon-keyed line whose value is
//!   empty (`anchors:`) or a block-scalar introducer (`|` / `|-` / `|+`) — the target is that line's
//!   own indent **+ 2**;
//! * otherwise (a colon-keyed line with an inline value, e.g. `id: main`) the target is that line's
//!   own indent, unchanged.
//!
//! This is deliberately a **local**, one-line lookback — not the whole-document depth accumulation
//! [`crate::diagnostics::indentation_pass`] performs to validate depth jumps, and it matches how
//! on-type indentation providers for other indentation-syntax languages behave: pressing Enter only
//! ever proposes the same depth or one deeper than the line just typed. Returning to a shallower
//! level is always a separate, user-driven action (Backspace / Shift+Tab), never guessed here. On a
//! canonical (already 2-space) document with no such dedent between two consecutive lines, this
//! locally agrees exactly with what [`format::format_line_edits`] would produce for the deeper line —
//! see the `agrees_with_format_line_edits_on_clean_document` test, which is the situation on-type
//! formatting actually needs (continuing at, or descending from, the previous line). Line 0 always
//! targets 0.
//!
//! ## When it refuses to guess (`None`)
//!
//! * The line sits inside a block-scalar body (`@onClick: |` followed by an indented Lua body): that
//!   indentation is **content**, not structure — [`format`](crate::format) itself emits block-scalar
//!   bodies byte-for-byte, so silently reindenting it would be data loss.
//! * The line itself, or the reference line the target would be computed from, is tab-indented: the
//!   engine hard-errors on tab indentation (spec §2.1) and there is already a dedicated
//!   `tab-indentation` diagnostic + quick fix for that; on-type formatting must not silently paper
//!   over it with a guessed space count.

/// One physical line of the source, sliced without its trailing `\n` (a trailing `\r` is kept and
/// treated as ordinary trailing whitespace, matching the engine's right-trim). Shared by
/// [`crate::diagnostics`] and this module — the crate's one definition of "a line."
pub(crate) struct Line<'a> {
    /// Byte offset of the line's first character within the source.
    pub(crate) start: usize,
    /// The line text, excluding the terminating `\n`.
    pub(crate) text: &'a str,
}

/// Splits `source` into lines carrying their byte offsets. A trailing `\n` does not produce a final
/// empty line; an interior blank line does (so blank lines are preserved).
pub(crate) fn split_lines(source: &str) -> Vec<Line<'_>> {
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
pub(crate) fn leading_spaces(text: &str) -> usize {
    text.bytes().take_while(|&b| b == b' ').count()
}

/// Whether `text`'s leading whitespace contains a tab (checked right after its leading spaces, same
/// position `getLineDepth` inspects).
pub(crate) fn has_tab_indent(text: &str) -> bool {
    let sp = leading_spaces(text);
    text.as_bytes().get(sp) == Some(&b'\t')
}

/// The value portion of a structural line, used to detect block-scalar markers (so their raw content
/// lines can be skipped) and, here, to classify whether a line opens a block. Mirrors the tag/value
/// split of `parseNode` closely enough for that purpose (list items via a leading `-`, otherwise the
/// text after the first `:`).
pub(crate) fn line_value(trimmed: &str) -> &str {
    if let Some(rest) = trimmed.strip_prefix('-') {
        return rest.trim();
    }
    match trimmed.find(':') {
        Some(pos) => trimmed[pos + 1..].trim(),
        None => "",
    }
}

pub(crate) fn is_block_scalar_marker(value: &str) -> bool {
    matches!(value, "|" | "|-" | "|+")
}

pub(crate) fn is_comment(trimmed: &str) -> bool {
    trimmed.starts_with("//") || trimmed.starts_with('#')
}

/// Whether a structural line (already known non-blank, non-comment) **opens a block** — i.e. the
/// following, more deeply indented lines are its children.
///
/// A list item (`- foo`) is always a leaf, per spec: never a block opener. Everything else is
/// decided purely from whether the line has a `:` and, if so, its value:
///
/// * no `:` at all → a bare container tag (`Widget`) or a `Name < Base` style header — both open a
///   block (neither ever carries a colon).
/// * `:` with an empty value → a `$state` selector (`$hover:`) or a colon-keyed block header
///   (`anchors:`) — opens a block.
/// * `:` with a block-scalar introducer value (`|`, `|-`, `|+`) → opens a block (the raw body).
/// * `:` with any other (non-empty, non-block-scalar) value (`id: main`) → a leaf; does not open a
///   block.
fn opens_block(trimmed: &str) -> bool {
    if trimmed.starts_with('-') {
        return false;
    }
    match trimmed.find(':') {
        None => true,
        Some(pos) => {
            let value = trimmed[pos + 1..].trim();
            value.is_empty() || is_block_scalar_marker(value)
        }
    }
}

/// How one physical line classifies for [`indent_for_line`]'s purposes.
enum LineKind {
    /// Whitespace-only (or empty).
    Blank,
    /// A full-line `//` or `#` comment.
    Comment,
    /// Raw content inside an open block-scalar body — not structure.
    BlockContent,
    /// A genuine structural node line.
    Structural {
        /// Its own leading-space count.
        sp: usize,
        /// Whether it opens a block (see [`opens_block`]).
        opens_block: bool,
        /// Whether its leading whitespace contains a tab.
        has_tab: bool,
    },
}

/// Classify every line of `lines`, mirroring the block-scalar-body skip
/// [`crate::diagnostics::indentation_pass`] performs: a block-scalar marker line's more-deeply
/// indented (and any intervening blank) successor lines are consumed as [`LineKind::BlockContent`]
/// rather than structure, exactly like the diagnostics pass and the formatter agree a body must be
/// treated. Returns exactly one [`LineKind`] per entry of `lines`, in order.
fn classify_lines(lines: &[Line<'_>]) -> Vec<LineKind> {
    let mut kinds = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let trimmed = line.text.trim();
        if trimmed.is_empty() {
            kinds.push(LineKind::Blank);
            i += 1;
            continue;
        }
        if is_comment(trimmed) {
            kinds.push(LineKind::Comment);
            i += 1;
            continue;
        }

        let sp = leading_spaces(line.text);
        kinds.push(LineKind::Structural {
            sp,
            opens_block: opens_block(trimmed),
            has_tab: has_tab_indent(line.text),
        });
        i += 1;

        // Block scalars: everything more deeply indented (or blank) than the marker line is raw
        // body content, not structure. Same rule, same rationale, as `indentation_pass`.
        if is_block_scalar_marker(line_value(trimmed)) {
            while i < lines.len() {
                let content = &lines[i];
                if content.text.trim().is_empty() {
                    kinds.push(LineKind::BlockContent);
                    i += 1;
                    continue;
                }
                if leading_spaces(content.text) > sp {
                    kinds.push(LineKind::BlockContent);
                    i += 1;
                } else {
                    break; // next structural line — reprocessed by the outer loop
                }
            }
        }
    }
    kinds
}

/// The number of leading SPACES line `line` (0-based) should have, computed purely lexically from
/// the preceding lines — never from the CST, so this keeps working on a mid-edit document a full
/// parse would choke on. See the module docs for the rule and for when this refuses to guess
/// (`None`).
///
/// `line` need not exist yet in `source` (e.g. pressing Enter at end-of-file produces a document
/// whose trailing `\n` has no line after it): in that case the lookback starts from the last real
/// line, exactly as if `line` were freshly appended and still blank.
#[must_use]
pub fn indent_for_line(source: &str, line: u32) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }

    let lines = split_lines(source);
    let kinds = classify_lines(&lines);
    let idx = line as usize;

    // A line that already exists and is itself raw block-scalar content, or is itself
    // tab-indented, is never reindented — see the module docs.
    match kinds.get(idx) {
        Some(LineKind::BlockContent) => return None,
        Some(LineKind::Structural { has_tab: true, .. }) => return None,
        _ => {}
    }

    // Walk backward from the line just before `idx` (or, when `idx` is past the end of the
    // document, from the last real line) for the nearest structural line, skipping blank and
    // comment lines.
    let mut i = idx.min(lines.len());
    while i > 0 {
        i -= 1;
        match kinds[i] {
            LineKind::Blank | LineKind::Comment => continue,
            // The nearest preceding content is raw block-scalar body: `line` sits inside (or,
            // still blank, immediately continues) an open body — never reindent it.
            LineKind::BlockContent => return None,
            LineKind::Structural {
                sp,
                opens_block,
                has_tab,
            } => {
                // The reference line's own indent is ambiguous (tabs do not count as spaces), so
                // do not compute a target from it.
                if has_tab {
                    return None;
                }
                return Some(if opens_block { sp + 2 } else { sp });
            }
        }
    }
    // Only blank/comment lines (or nothing) precede `line`: top level.
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;

    #[test]
    fn first_line_is_always_zero() {
        assert_eq!(indent_for_line("", 0), Some(0));
        assert_eq!(indent_for_line("Panel\n  id: main\n", 0), Some(0));
    }

    #[test]
    fn after_a_style_header_indents_one_level() {
        let src = "MainWindow < UIWindow\n";
        assert_eq!(indent_for_line(src, 1), Some(2));
    }

    #[test]
    fn after_a_bare_container_tag_indents_one_level() {
        let src = "Panel\n";
        assert_eq!(indent_for_line(src, 1), Some(2));
    }

    #[test]
    fn after_an_inline_valued_property_keeps_the_same_indent() {
        let src = "Panel\n  id: main\n";
        assert_eq!(indent_for_line(src, 2), Some(2));
    }

    #[test]
    fn after_an_empty_valued_colon_key_indents_one_level() {
        // `anchors:` (block form) has no inline value: it opens a block.
        let src = "Panel\n  anchors:\n";
        assert_eq!(indent_for_line(src, 2), Some(4));
    }

    #[test]
    fn after_a_state_selector_indents_one_level() {
        let src = "Panel\n  $hover:\n";
        assert_eq!(indent_for_line(src, 2), Some(4));
    }

    #[test]
    fn blank_and_comment_lines_are_skipped_when_looking_backward() {
        let src = "\
Panel
  id: main

  // a comment

";
        // Line 5 (blank) should still look past the comment and the blank line to `id: main`.
        assert_eq!(indent_for_line(src, 5), Some(2));
    }

    #[test]
    fn a_broken_mid_edit_document_still_produces_an_answer() {
        // An unterminated inline array on the previous line: this does not parse cleanly (an
        // ERROR node), yet the lexical rule still has an answer — `x:` is a colon-keyed line with
        // a non-empty (albeit malformed) value, so it does not open a block.
        let src = "Panel\n  x: [a, b\n";
        assert_eq!(indent_for_line(src, 2), Some(2));

        // Enter pressed right after an unterminated style header line: no trailing content line
        // exists yet in `source`, but the lookback still resolves from the last real line.
        let src = "MainWindow < \n";
        assert_eq!(indent_for_line(src, 1), Some(2));
    }

    #[test]
    fn enter_at_end_of_file_with_no_trailing_line_still_resolves() {
        // `source` ends with a `\n` and carries no line after it (per `split_lines`'s contract);
        // `line` (1) is past the end of the document, exactly the state right after pressing Enter
        // at EOF.
        let src = "Panel\n";
        assert_eq!(indent_for_line(src, 1), Some(2));
    }

    #[test]
    fn inside_a_block_scalar_body_refuses_to_guess() {
        let src = "\
Panel
  @onClick: |
    self:hide()
";
        // The (blank) line right after the marker, still inside the open body.
        assert_eq!(indent_for_line(src, 2), None);
        // A line already holding block-scalar content.
        let src2 = "Panel\n  @onClick: |\n    one()\n    two()\n";
        assert_eq!(indent_for_line(src2, 3), None);
    }

    #[test]
    fn a_fresh_blank_line_right_after_a_block_scalar_body_stays_open() {
        // Pressing Enter right after the last body line, with nothing following yet: the body is
        // still open (blank lines keep it open, mirroring `indentation_pass`), so this must not
        // guess an indentation for what might still become body content.
        let src = "Panel\n  @onClick: |\n    self:hide()\n";
        assert_eq!(indent_for_line(src, 3), None);
    }

    #[test]
    fn tab_indented_reference_line_refuses_to_guess() {
        let src = "Panel\n\tid: main\n";
        assert_eq!(indent_for_line(src, 2), None);
    }

    #[test]
    fn tab_indented_target_line_refuses_to_guess() {
        let src = "Panel\n  id: main\n\tsize: 1 2\n";
        assert_eq!(indent_for_line(src, 2), None);
    }

    #[test]
    fn agrees_with_format_line_edits_on_clean_document() {
        // On a clean, already-canonical document where depth never decreases from one line to the
        // next (every consecutive pair is either a sibling — same depth — or a first child — one
        // level deeper, exactly what pressing Enter can ever need), `indent_for_line` must compute
        // exactly the indentation `format_line_edits` would produce for that line: the same one
        // indentation rule, never two subtly different ones. (A dedent, e.g. `@onClick:` returning
        // to `Button`'s level after `$hover:`'s nested `color:`, is a separate, user-driven action —
        // see the module docs — and is deliberately outside this test's claim.)
        let src = "\
MainWindow < UIWindow
  id: main
  size: 100 200
  Button
    id: ok
    $hover:
      color: red
      width: 5
";
        let lines: Vec<&str> = src.lines().collect();
        assert!(
            format::format_line_edits(src, 0, lines.len() as u32).expect("formats") == Vec::new(),
            "fixture must already be canonical"
        );
        for (i, text) in lines.iter().enumerate().skip(1) {
            let expected =
                indent_for_line(src, i as u32).expect("no block scalars in this fixture");
            let actual_indent = text.len() - text.trim_start().len();
            assert_eq!(
                expected, actual_indent,
                "line {i} ({text:?}): indent_for_line={expected}, formatter={actual_indent}"
            );
        }
    }
}
