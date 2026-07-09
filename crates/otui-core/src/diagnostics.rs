//! Parse-level diagnostics (spec §4), a faithful mirror of the OTClient OTML parser.
//!
//! This is the *parse* category only — every finding here is a [`Severity::Error`], because these
//! are conditions the real engine treats as fatal (`OTMLException`) or that leave the tree-sitter
//! grammar unable to form a valid node. Higher-level, engine-tolerated authoring mistakes
//! (unknown properties, unknown `$state`, style-resolution warnings) are *hints/warnings* handled
//! by later milestones and are intentionally **not** produced here.
//!
//! Two independent passes contribute:
//!
//! 1. A **line-based indentation** pass mirroring `OTMLParser::getLineDepth` / `parseLine`:
//!    tabs in leading whitespace, odd (non-multiple-of-2) indentation, and invalid depth jumps.
//! 2. A **structural** pass harvesting tree-sitter `ERROR` / `MISSING` nodes for malformed
//!    constructs (e.g. an unterminated inline array).

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

/// Computes all parse-level diagnostics for `source`.
///
/// Returns findings sorted by span (`start`, then `end`). The document is parsed once; the two
/// passes share nothing beyond the source text.
#[must_use]
pub fn analyze(source: &str) -> Vec<Diagnostic> {
    let mut out = indentation_pass(source);
    if let Some(tree) = SyntaxTree::parse(source) {
        collect_structural_errors(tree.root(), &mut out);
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
        let src = "\
a:
  b:
    c:
      d: 1
";
        assert!(analyze(src).is_empty());
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
}
