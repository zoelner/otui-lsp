//! Whole-document formatting (spec §8): a conservative, safety-first OTML formatter.
//!
//! OTML indentation is **structure** (exactly 2 spaces per depth level) and a property value runs
//! verbatim to end-of-line, so a formatter must never change meaning — it only normalizes
//! whitespace. Getting this wrong corrupts user files, so the scope is deliberately tight and the
//! pass prefers doing nothing over risking a bad edit.
//!
//! ## What [`format`] does, and nothing else
//!
//! Operating line by line, guided by the tree-sitter CST for each structural line's **depth**:
//!
//! 1. **Leading indentation** of every structural line → exactly `2 * depth` spaces, where `depth`
//!    is the node's nesting depth in the parsed tree (its number of statement-node ancestors), not
//!    a function of the authored indentation. This is what normalizes over-indented,
//!    under-indented, odd, and tab-indented structural lines to the canonical 2-space grid.
//! 2. **`key: value` spacing** on a colon-keyed property line → exactly one space after the
//!    **first** `:` (the grammar's key/value separator token). `key:value` / `key:   value` collapse
//!    to `key: value`; a `key:` with no value (a block header or a bare key) stays `key:`. Nothing
//!    after the first colon is touched — the value is kept verbatim (an inline `//` / `#`, an
//!    embedded `:`, an `http://` URL are all data). Applied to `property`, `id_property`,
//!    `anchor_property`, and the `@event` / `&alias` / `!expr` Lua-bearing properties. **Not**
//!    applied to `$state` selector headers, `Name < Base` style headers, bare container tags, or
//!    `- list items` — their colons (if any) are not key/value separators.
//! 3. **Trailing whitespace** on every structural / comment line → stripped.
//! 4. **Final newline** → the document ends with exactly one `\n`.
//! 5. **Comment lines** keep their content intact; only their leading indent (rule 1, using the
//!    depth of the block the parser places them in) and trailing whitespace (rule 3) are touched.
//!    **Blank lines** stay blank (no indentation is emitted). No blank lines are added or removed
//!    (bar collapsing trailing blank lines into the single final newline of rule 4), nothing is
//!    sorted, and inline-array internal spacing (`[a,b, c]`) is left as authored.
//!
//! ## Raw regions left byte-for-byte untouched
//!
//! A block scalar body (`|` / `|-` / `|+`) carries raw multi-line Lua; its lines — content **and**
//! indentation, including interior blank lines — are emitted verbatim. Any other non-blank line
//! that does not *start* a structural node (e.g. the continuation lines of a multi-line inline
//! array) is likewise left verbatim: only lines the CST identifies as a statement/comment header
//! are re-indented, so a line whose meaning we do not fully model is never rewritten.
//!
//! ## Hard safety gate
//!
//! If [`SyntaxTree::parse`] fails, or the parsed tree contains any `ERROR` / `MISSING` node,
//! [`format`] returns [`None`] and the caller makes no edit. A document that does not parse cleanly
//! is never reformatted. The pass is also **idempotent**: `format(format(x)) == format(x)`.

use std::collections::HashMap;

use crate::syntax::SyntaxTree;
use tree_sitter::Node;

/// Format `source` into the canonical whole-document text, or [`None`] when the document does not
/// parse cleanly (parse failure, or any `ERROR` / `MISSING` node in the tree) — in which case the
/// caller must leave the document untouched. See the module docs for the exact normalizations.
#[must_use]
pub fn format(source: &str) -> Option<String> {
    let tree = SyntaxTree::parse(source)?;
    // Hard safety gate: never reformat a document that does not parse cleanly.
    if tree.has_error() {
        return None;
    }

    let lines = split_lines(source);
    let line_starts: Vec<usize> = lines.iter().map(|l| l.start).collect();

    // Per-line metadata for the lines that *start* a structural node or a comment, plus the byte
    // spans of block-scalar bodies (whose lines are emitted verbatim).
    let mut meta: HashMap<usize, LineMeta> = HashMap::new();
    let mut block_scalar_spans: Vec<(usize, usize)> = Vec::new();
    collect(
        tree.root(),
        &line_starts,
        0,
        &mut meta,
        &mut block_scalar_spans,
    );

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        // 1. A block-scalar body line is raw: emit it byte-for-byte (content AND indentation,
        //    including interior blank lines). This takes precedence over every other rule.
        if in_block_scalar(line.start, &block_scalar_spans) {
            out.push(line.text.to_owned());
            continue;
        }
        // 2. A line that starts a structural node or a comment: re-indent (and, for a colon
        //    property, normalize the single space after the first colon).
        if let Some(m) = meta.get(&i) {
            out.push(format_header_line(line, m));
            continue;
        }
        // 3. A blank line stays blank, with no indentation.
        if line.text.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        // 4. Any other non-blank line does not start a node and is not a block-scalar body: it is a
        //    value continuation (e.g. a multi-line inline array). Leave it verbatim — the formatter
        //    only rewrites lines whose structure the CST fully identifies.
        out.push(line.text.to_owned());
    }

    // Ensure exactly one trailing newline (rule 4), collapsing any trailing blank lines into it —
    // EXCEPT when the document ends inside a block-scalar body. A `|+` (keep-chomping) block scalar
    // at EOF carries trailing blank lines that are semantically part of its body (spec §2.1); those
    // trailing blanks fall *after* the `block_scalar_content` token but are still block-scalar body,
    // so they must survive byte-for-byte like the rest of the body, never be collapsed.
    let last_content = lines.iter().rposition(|l| !l.text.trim().is_empty());
    let ends_in_block_scalar_tail = match last_content {
        // The last non-blank line is a block-scalar body line AND at least one blank line follows it
        // (a trailing blank the ordinary collapse would otherwise delete).
        Some(idx) => {
            idx + 1 < lines.len() && in_block_scalar(lines[idx].start, &block_scalar_spans)
        }
        None => false,
    };

    if ends_in_block_scalar_tail {
        // Preserve the block scalar's trailing blank lines exactly. Re-render everything up to and
        // including the last body line (`out[idx]` is that body line, emitted verbatim by rule 1),
        // then append the original source bytes from the end of that line to EOF — its newline plus
        // the trailing blank lines, byte-for-byte, including any final-newline state.
        let idx = last_content.expect("a block-scalar tail implies a last content line");
        let head = out[..=idx].join("\n");
        let tail = &source[lines[idx].start + lines[idx].text.len()..];
        return Some(format!("{head}{tail}"));
    }

    // An empty (or whitespace-only) document formats to the empty string rather than a lone newline.
    let joined = out.join("\n");
    let trimmed = joined.trim_end_matches('\n');
    if trimmed.is_empty() {
        return Some(String::new());
    }
    Some(format!("{trimmed}\n"))
}

/// Metadata for a source line that begins a structural node or a comment.
struct LineMeta {
    /// The node's nesting depth (number of statement-node ancestors) — the line's leading
    /// indentation becomes `2 * depth` spaces.
    depth: usize,
    /// The byte offset of the line's first non-whitespace character (the node start).
    content_start: usize,
    /// For a colon-keyed property, the byte offset just past its first `:` separator token, so the
    /// single-space-after-colon rule can be applied precisely. [`None`] for comments, `$state`
    /// selectors, style headers, containers, and list items (their colons, if any, are not
    /// key/value separators).
    colon_end: Option<usize>,
}

/// One physical line of the source, sliced without its terminating `\n` (a trailing `\r` is kept as
/// ordinary trailing whitespace and is stripped from structural lines by rule 3).
struct Line<'a> {
    /// Byte offset of the line's first character within the source.
    start: usize,
    /// The line text, excluding the terminating `\n`.
    text: &'a str,
}

/// Split `source` into lines carrying their byte offsets. A trailing `\n` does not produce a final
/// empty line; an interior blank line does (so blank lines are preserved).
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

/// Grammar node kinds that are structural statements (each occupies one line and carries a depth).
fn is_statement(kind: &str) -> bool {
    matches!(
        kind,
        "style_header"
            | "state_selector"
            | "event_property"
            | "alias_property"
            | "expr_property"
            | "anchor_property"
            | "id_property"
            | "list_item"
            | "property"
            | "container"
    )
}

/// Statement kinds whose `:` is a key/value separator, so the single-space-after-colon rule
/// applies. `state_selector` (its `:` terminates the selector), `style_header`, `container`, and
/// `list_item` are deliberately excluded.
fn is_colon_property(kind: &str) -> bool {
    matches!(
        kind,
        "property"
            | "id_property"
            | "anchor_property"
            | "event_property"
            | "alias_property"
            | "expr_property"
    )
}

/// Depth-first walk recording, for each line that starts a statement or comment, its depth (number
/// of statement-node ancestors), node start, and colon position; and collecting block-scalar body
/// spans. `depth` is the statement depth to assign to `node`'s direct statement/comment children.
fn collect(
    node: Node<'_>,
    line_starts: &[usize],
    depth: usize,
    meta: &mut HashMap<usize, LineMeta>,
    block_scalar_spans: &mut Vec<(usize, usize)>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "block_scalar_content" {
            block_scalar_spans.push((child.start_byte(), child.end_byte()));
            continue;
        }
        if is_statement(kind) {
            let line = line_of(line_starts, child.start_byte());
            let colon_end = if is_colon_property(kind) {
                first_colon_end(child)
            } else {
                None
            };
            meta.insert(
                line,
                LineMeta {
                    depth,
                    content_start: child.start_byte(),
                    colon_end,
                },
            );
            // Descend: this node's own statement/comment children sit one level deeper.
            collect(child, line_starts, depth + 1, meta, block_scalar_spans);
        } else if kind == "comment" {
            // A comment is indentation-neutral in the parser; it belongs to the block of the next
            // real line, which is exactly the depth at which the CST attaches it here.
            let line = line_of(line_starts, child.start_byte());
            meta.insert(
                line,
                LineMeta {
                    depth,
                    content_start: child.start_byte(),
                    colon_end: None,
                },
            );
        } else {
            // A field/value/punctuation node: not a statement, keep the same depth.
            collect(child, line_starts, depth, meta, block_scalar_spans);
        }
    }
}

/// The byte offset just past `node`'s first direct `:` child (the key/value separator), if present.
fn first_colon_end(node: Node<'_>) -> Option<usize> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == ":" {
            return Some(child.end_byte());
        }
    }
    None
}

/// The index of the line containing byte `offset`: the last line whose start is `<= offset`.
fn line_of(line_starts: &[usize], offset: usize) -> usize {
    line_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1)
}

/// Whether `line_start` falls inside a block-scalar body span. A body span begins at the newline
/// ending the marker line, so a body line's start is strictly greater than the span start and no
/// greater than its end.
fn in_block_scalar(line_start: usize, spans: &[(usize, usize)]) -> bool {
    spans
        .iter()
        .any(|&(s, e)| line_start > s && line_start <= e)
}

/// Re-render a structural / comment header `line` at its canonical indentation, applying the
/// single-space-after-colon rule for a colon property and stripping trailing whitespace.
fn format_header_line(line: &Line<'_>, m: &LineMeta) -> String {
    let indent = "  ".repeat(m.depth);
    match m.colon_end {
        Some(colon_end) => {
            // `key:` (verbatim from the first non-whitespace char through the colon) followed by at
            // most one space and the verbatim rest-of-line value (trimmed at both ends: a leading
            // trim realizes the single space, a trailing trim is rule 3).
            let cs = m.content_start - line.start;
            let ce = colon_end - line.start;
            let key = &line.text[cs..ce];
            let value = line.text[ce..].trim();
            if value.is_empty() {
                format!("{indent}{key}")
            } else {
                format!("{indent}{key} {value}")
            }
        }
        // A comment, or a non-colon statement (state selector, style header, container, list item):
        // re-indent and strip trailing whitespace; the content is otherwise untouched.
        None => format!("{indent}{}", line.text.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Format `src`, expecting it to parse cleanly (panics otherwise).
    fn fmt(src: &str) -> String {
        format(src).expect("clean document formats")
    }

    #[test]
    fn already_canonical_document_is_unchanged() {
        let src = "\
MainWindow < UIWindow
  id: main
  size: 100 200
  Button
    id: ok
    @onClick: |
      self:hide()
";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn over_indented_structural_lines_are_normalized() {
        // `id` is one level under `Panel` but authored with four spaces.
        let src = "Panel\n    id: main\n";
        assert_eq!(fmt(src), "Panel\n  id: main\n");
    }

    #[test]
    fn under_indented_structural_lines_are_normalized() {
        // A single-space (odd) indent still nests one level per the tree → 2 spaces.
        let src = "Panel\n id: main\n";
        assert_eq!(fmt(src), "Panel\n  id: main\n");
    }

    #[test]
    fn tab_indented_structural_lines_are_normalized() {
        // A tab counts as one indent unit in the scanner → depth 1 → two spaces.
        let src = "Panel\n\tid: main\n";
        assert_eq!(fmt(src), "Panel\n  id: main\n");
    }

    #[test]
    fn deep_nesting_normalized_to_two_spaces_per_level() {
        let src = "A\n      b:\n            c: 1\n";
        assert_eq!(fmt(src), "A\n  b:\n    c: 1\n");
    }

    #[test]
    fn key_value_spacing_is_collapsed_to_one_space() {
        assert_eq!(fmt("Panel\n  width:10\n"), "Panel\n  width: 10\n");
        assert_eq!(fmt("Panel\n  width:   10\n"), "Panel\n  width: 10\n");
        assert_eq!(fmt("Panel\n  width: 10\n"), "Panel\n  width: 10\n");
    }

    #[test]
    fn bare_key_and_block_header_keep_no_trailing_space() {
        // A colon with no value stays `key:` (no space appended), both for a block header and a
        // block-scalar marker line.
        assert_eq!(fmt("layout:\n  type: grid\n"), "layout:\n  type: grid\n");
        assert_eq!(
            fmt("W\n  @onClick: |\n    a()\n"),
            "W\n  @onClick: |\n    a()\n"
        );
    }

    #[test]
    fn value_after_first_colon_is_kept_verbatim() {
        // An embedded `:` and a trailing `//`/`#` inside a value are data, preserved exactly after
        // the single normalized space.
        let src = "W\n  image-source:  /path/a:b // note\n";
        assert_eq!(fmt(src), "W\n  image-source: /path/a:b // note\n");
        let hash = "W\n  text:  a # b : c\n";
        assert_eq!(fmt(hash), "W\n  text: a # b : c\n");
    }

    #[test]
    fn trailing_whitespace_is_stripped() {
        let src = "Panel   \n  id: main\t \n";
        assert_eq!(fmt(src), "Panel\n  id: main\n");
    }

    #[test]
    fn final_newline_is_ensured() {
        assert_eq!(fmt("Panel\n  id: main"), "Panel\n  id: main\n");
        assert_eq!(fmt("Panel\n  id: main\n\n\n"), "Panel\n  id: main\n");
    }

    #[test]
    fn ordinary_document_collapses_trailing_blank_lines() {
        // Guard: with no trailing block scalar, trailing blank lines still collapse into a single
        // final newline (scoping the block-scalar-tail exception correctly).
        assert_eq!(fmt("Panel\n  id: a\n\n\n\n"), "Panel\n  id: a\n");
        // The over-indented `id` is also normalized; only the trailing blanks are collapsed.
        assert_eq!(fmt("Panel\n    id: a\n  \n\n"), "Panel\n  id: a\n");
    }

    #[test]
    fn keep_chomped_block_scalar_trailing_blanks_at_eof_are_preserved() {
        // A `|+` block scalar at EOF keeps its trailing blank lines as body content; they must
        // survive byte-for-byte rather than collapse into a single final newline. The (over-indented)
        // marker line is still normalized to depth 1, but the body and its trailing blanks are exact.
        let src = "Panel\n    @onClick: |+\n        body()\n\n\n";
        let out = fmt(src);
        assert_eq!(out, "Panel\n  @onClick: |+\n        body()\n\n\n");
        // The two trailing blank lines survive (three trailing newlines).
        assert!(out.ends_with("body()\n\n\n"), "{out:?}");
        // And re-formatting is a no-op (idempotent), never eroding the trailing blanks further.
        assert_eq!(fmt(&out), out);
    }

    #[test]
    fn plain_block_scalar_at_eof_keeps_its_trailing_blank_lines_verbatim() {
        // The same protection applies to any block scalar whose body reaches EOF: the trailing blank
        // lines are part of its (byte-for-byte) body, and a blank line carrying stray spaces is kept
        // exactly as authored.
        let src = "W\n  @x: |\n    a()\n   \n\n";
        assert_eq!(fmt(src), src);
        assert_eq!(fmt(src), fmt(&fmt(src)));
    }

    #[test]
    fn blank_lines_are_preserved_and_emptied() {
        // An interior blank line stays (blank), with any stray indentation removed.
        let src = "Panel\n  id: a\n   \n  id: b\n";
        assert_eq!(fmt(src), "Panel\n  id: a\n\n  id: b\n");
    }

    #[test]
    fn empty_and_whitespace_only_documents_format_to_empty() {
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("   \n\n"), "");
    }

    #[test]
    fn state_selector_and_style_header_colons_are_untouched() {
        // The `:` terminating a `$state` selector, and a `Name < Base` header, are re-indented only
        // (no key/value spacing rule).
        let src = "Button\n  $hover !disabled:\n    color:red\n";
        assert_eq!(fmt(src), "Button\n  $hover !disabled:\n    color: red\n");
        assert_eq!(fmt("MainWindow < UIWindow\n"), "MainWindow < UIWindow\n");
    }

    #[test]
    fn anchor_and_alias_and_event_colons_get_one_space() {
        let src = "W\n  anchors.top:parent.top\n  &c:#ff0000\n  !text:tr('x')\n";
        assert_eq!(
            fmt(src),
            "W\n  anchors.top: parent.top\n  &c: #ff0000\n  !text: tr('x')\n"
        );
    }

    #[test]
    fn block_scalar_body_is_left_byte_for_byte_unchanged() {
        // The Lua body's own (irregular) indentation and content must survive exactly, including an
        // interior blank line, even though it is neither a 2-space multiple nor structurally valid.
        let src = "\
Panel
  @onClick: |
       self:hide()

         foo(  bar )
  id: y
";
        let out = fmt(src);
        // The body lines are preserved verbatim; only the surrounding structural lines are touched.
        assert!(out.contains("\n       self:hide()\n"), "{out}");
        assert!(out.contains("\n\n         foo(  bar )\n"), "{out}");
        // And the whole document was formattable and stable here.
        assert_eq!(out, src);
    }

    #[test]
    fn block_scalar_body_is_not_reindented_even_when_marker_line_is() {
        // The marker and its sibling `id` are both authored one level under `Panel` but with four
        // spaces; the block body is deeper still (eight spaces) and carries trailing whitespace.
        let src = "Panel\n    @onClick: |\n        body:kept  \n    id: y\n";
        // The marker and `id` lines normalize to depth 1 (2 spaces), but the body line stays
        // verbatim (its eight-space indent and trailing whitespace preserved byte-for-byte).
        assert_eq!(
            fmt(src),
            "Panel\n  @onClick: |\n        body:kept  \n  id: y\n"
        );
    }

    #[test]
    fn multiline_inline_array_continuation_is_left_verbatim() {
        // The continuation line of a multi-line array is not a node header: it is left untouched,
        // while the header line's key/value spacing and indentation are normalized.
        let src = "W\n    x:[a,\n     b, c]\n";
        assert_eq!(fmt(src), "W\n  x: [a,\n     b, c]\n");
    }

    #[test]
    fn comment_lines_are_reindented_to_their_block_depth() {
        // A comment before a depth-1 line belongs to that block → 2 spaces; a comment before a
        // depth-0 line belongs to the top level → 0 spaces. Content is otherwise intact.
        let src = "Panel\n// keep me\n  id: main\n   # trailing note\nWidget\n";
        assert_eq!(
            fmt(src),
            "Panel\n  // keep me\n  id: main\n# trailing note\nWidget\n"
        );
    }

    #[test]
    fn list_items_are_reindented_without_touching_their_dash() {
        let src = "items:\n    - one\n    - two\n";
        assert_eq!(fmt(src), "items:\n  - one\n  - two\n");
    }

    #[test]
    fn a_document_with_a_parse_error_yields_none() {
        // An unterminated inline array produces an ERROR node → the safety gate returns None so the
        // server makes no edit.
        assert!(format("x: [a, b\n").is_none());
    }

    #[test]
    fn formatting_is_idempotent() {
        let inputs = [
            "Panel\n    id: main\n\twidth:10\n",
            "MainWindow < UIWindow\n id: main\n  Button\n      id: ok\n   @onClick: |\n         self:hide()\n",
            "Button\n  $hover:\n      color:red\n  anchors.top:parent.top\n",
            "items:\n    - one\n\n    - two\n",
            "W\n  x:[a,\n     b, c]\n// note\n",
            "Panel\n  @onClick: |+\n    body()\n\n\n",
            "",
            "   \n\n",
        ];
        for src in inputs {
            let once = format(src).expect("formats");
            let twice = format(&once).expect("re-formats");
            assert_eq!(once, twice, "not idempotent for {src:?}");
        }
    }
}
