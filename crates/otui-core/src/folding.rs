//! Folding ranges (spec §2): the collapsible regions an editor shows in its gutter.
//!
//! This pass walks the tree-sitter [`SyntaxTree`] and emits one [`FoldRange`] per collapsible
//! construct, expressed purely in **0-based line numbers** (no `lsp-types`, no I/O). Two structural
//! sources fold:
//!
//! * every **widget block** — a `container` or `style_header` (the structural nodes of the grammar,
//!   see [`crate::symbols`]) whose indented body makes it span more than one line. Nested widgets
//!   each fold on their own, since each is its own node in the tree.
//! * every multi-line **block-scalar body** (`|` / `|-` / `|+`, spec §2): from the marker line to
//!   the last body line.
//!
//! A run of **two or more consecutive full-line comments** additionally folds into a single
//! [`FoldKind::Comment`] region; everything else is a [`FoldKind::Region`].
//!
//! ## Line semantics
//!
//! A [`FoldRange`] folds lines `start_line..=end_line`: the client keeps `start_line` (the header /
//! marker line) visible and hides its body down through `end_line` (the last body line). A construct
//! that fits on a single line therefore yields **no** fold (`end_line == start_line`), so single-line
//! widgets, scalar properties and lone comments never appear.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;

/// The category of a foldable region, mirroring the LSP `FoldingRangeKind` the server maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldKind {
    /// A structural region: a widget block or a multi-line block-scalar body.
    Region,
    /// A run of consecutive full-line comments.
    Comment,
}

/// A foldable range of lines, both endpoints **0-based and inclusive**: the fold covers
/// `start_line..=end_line`, keeping `start_line` visible and hiding through `end_line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRange {
    /// The first line of the fold (the header / marker line), kept visible when collapsed.
    pub start_line: u32,
    /// The last line of the fold (the last body line), hidden when collapsed. Always `> start_line`.
    pub end_line: u32,
    /// Whether this is a structural [`Region`](FoldKind::Region) or a [`Comment`](FoldKind::Comment)
    /// run.
    pub kind: FoldKind,
}

/// Compute the folding ranges for `source` (see the module docs).
///
/// Returns an empty vector when the source cannot be parsed, when the parse tree contains any
/// `ERROR`/`MISSING` node, or when it holds no multi-line construct (a flat document folds nowhere).
///
/// Folding is gated on a clean parse — matching the formatter's safety check — because tree-sitter's
/// error recovery can reparent nodes to the wrong depth, which would yield folds spanning the wrong
/// lines. Rather than fold a misrecovered tree, we fold nothing until the document parses cleanly.
#[must_use]
pub fn folding_ranges(source: &str) -> Vec<FoldRange> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    if tree.has_error() {
        return Vec::new();
    }
    let line_starts = line_starts(source);
    let mut out = Vec::new();
    let mut comment_lines: Vec<u32> = Vec::new();

    // One pre-order pass over the whole tree: structural nodes fold directly; comment lines are
    // gathered and grouped into runs afterwards.
    tree.walk(|kind, span| match kind {
        "container" | "style_header" | "block_scalar" => {
            if let Some(fold) = region_fold(span, &line_starts) {
                out.push(fold);
            }
        }
        "comment" => comment_lines.push(line_of(span.start, &line_starts)),
        _ => {}
    });

    out.extend(comment_folds(&mut comment_lines));
    out
}

/// The byte offset at which each line begins (index 0 is always `0`). A trailing newline adds a
/// final entry for the empty last line, which never matters here since folds derive from node spans.
fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// The 0-based line containing byte `offset`: the last line whose start is `<= offset`.
fn line_of(offset: usize, line_starts: &[usize]) -> u32 {
    // `partition_point` gives the first index whose start is `> offset`; the line is the one before.
    (line_starts.partition_point(|&start| start <= offset) - 1) as u32
}

/// Build a structural [`FoldKind::Region`] fold for a node `span`, or `None` when it fits on one
/// line.
///
/// The end line is taken from the node's **last byte** (`end` is exclusive), so a span that reaches
/// to the line terminator folds to that content line rather than spilling onto the next one.
fn region_fold(span: ByteSpan, line_starts: &[usize]) -> Option<FoldRange> {
    let start_line = line_of(span.start, line_starts);
    let last_byte = span.end.saturating_sub(1).max(span.start);
    let end_line = line_of(last_byte, line_starts);
    (end_line > start_line).then_some(FoldRange {
        start_line,
        end_line,
        kind: FoldKind::Region,
    })
}

/// Group the gathered comment `lines` into one [`FoldKind::Comment`] fold per maximal run of
/// consecutive lines of length >= 2. A lone comment (or comments separated by a blank/code line)
/// produces nothing. Sorts and dedups `lines` in place first.
fn comment_folds(lines: &mut Vec<u32>) -> Vec<FoldRange> {
    lines.sort_unstable();
    lines.dedup();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut j = i;
        while j + 1 < lines.len() && lines[j + 1] == lines[j] + 1 {
            j += 1;
        }
        if j > i {
            out.push(FoldRange {
                start_line: lines[i],
                end_line: lines[j],
                kind: FoldKind::Comment,
            });
        }
        i = j + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `FoldRange` for the widget/block whose fold starts on `start_line` (panics if absent).
    fn fold_starting_at(folds: &[FoldRange], start_line: u32) -> FoldRange {
        *folds
            .iter()
            .find(|f| f.start_line == start_line)
            .unwrap_or_else(|| panic!("expected a fold starting at line {start_line}: {folds:?}"))
    }

    #[test]
    fn widget_with_a_body_folds_from_header_to_last_body_line() {
        // line 0: Panel
        // line 1:   id: main
        // line 2:   text: Hi
        let src = "Panel\n  id: main\n  text: Hi\n";
        let folds = folding_ranges(src);
        assert_eq!(folds.len(), 1);
        assert_eq!(
            folds[0],
            FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Region,
            }
        );
    }

    #[test]
    fn nested_widget_yields_its_own_inner_fold_plus_the_outer() {
        // line 0: MainWindow < UIWindow
        // line 1:   id: main
        // line 2:   Panel
        // line 3:     id: content
        // line 4:     Button
        // line 5:       text: Click
        let src = "\
MainWindow < UIWindow
  id: main
  Panel
    id: content
    Button
      text: Click
";
        let folds = folding_ranges(src);
        // Outer style_header (0..5), inner Panel (2..5), inner Button (4..5).
        assert_eq!(folds.len(), 3);
        assert_eq!(fold_starting_at(&folds, 0).end_line, 5);
        assert_eq!(fold_starting_at(&folds, 2).end_line, 5);
        assert_eq!(fold_starting_at(&folds, 4).end_line, 5);
        assert!(folds.iter().all(|f| f.kind == FoldKind::Region));
    }

    #[test]
    fn single_line_widget_or_property_yields_no_fold() {
        // A bare one-line container and a lone property: neither spans more than a line.
        assert!(folding_ranges("Panel\n").is_empty());
        assert!(folding_ranges("width: 10\n").is_empty());
    }

    #[test]
    fn multi_line_block_scalar_body_folds() {
        // line 0: Button
        // line 1:   @onClick: |
        // line 2:     print(1)
        // line 3:     print(2)
        let src = "Button\n  @onClick: |\n    print(1)\n    print(2)\n";
        let folds = folding_ranges(src);
        // The block scalar spans lines 1..3; the enclosing Button spans 0..3.
        let scalar = fold_starting_at(&folds, 1);
        assert_eq!(scalar.end_line, 3);
        assert_eq!(scalar.kind, FoldKind::Region);
        let button = fold_starting_at(&folds, 0);
        assert_eq!(button.end_line, 3);
    }

    #[test]
    fn flat_document_yields_no_folds() {
        // Only single-line siblings, nothing nested: nothing to collapse.
        let src = "id: a\nwidth: 10\nheight: 20\n";
        assert!(folding_ranges(src).is_empty());
    }

    #[test]
    fn line_numbers_are_zero_based_with_correct_start_and_end() {
        // A leading blank line pushes the widget down: the header is line 1, the last body line 3.
        // line 0: (blank)
        // line 1: Panel
        // line 2:   id: main
        // line 3:   text: Hi
        let src = "\nPanel\n  id: main\n  text: Hi\n";
        let folds = folding_ranges(src);
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0].start_line, 1);
        assert_eq!(folds[0].end_line, 3);
    }

    #[test]
    fn a_run_of_consecutive_comments_folds_as_a_comment_region() {
        // line 0: // one
        // line 1: // two
        // line 2: // three
        // line 3: Panel
        let src = "// one\n// two\n// three\nPanel\n";
        let folds = folding_ranges(src);
        assert_eq!(folds.len(), 1);
        assert_eq!(
            folds[0],
            FoldRange {
                start_line: 0,
                end_line: 2,
                kind: FoldKind::Comment,
            }
        );
    }

    #[test]
    fn a_lone_comment_does_not_fold() {
        let src = "// just one\nPanel\n";
        assert!(folding_ranges(src).is_empty());
    }

    #[test]
    fn empty_source_has_no_folds() {
        assert!(folding_ranges("").is_empty());
    }

    #[test]
    fn a_document_with_a_parse_error_yields_no_folds() {
        // An unterminated inline array produces an ERROR node; folding is gated on a clean parse
        // (matching the formatter) so error-recovered, possibly-misplaced folds are never emitted —
        // even though the widget block above it would otherwise fold.
        let src = "Panel\n  id: main\n  items: [1, 2\n";
        assert!(folding_ranges(src).is_empty());
    }
}
