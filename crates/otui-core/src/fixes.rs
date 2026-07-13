//! Quick-fix computation (spec §7): protocol-agnostic one-click corrections derived from the
//! parse-level [`diagnostics`](crate::diagnostics).
//!
//! This module turns the diagnostics the engine already produces into concrete edits. It reuses the
//! same closed sets ([`schema`](crate::schema)) and catalogs ([`catalog`](crate::catalog)) the
//! diagnostics consume, plus a small edit-distance helper for the "did you mean" suggestions. It
//! decides nothing about presentation: it emits [`Fix`]es carrying byte-span replacements, and the
//! server maps those onto `lsp_types::CodeAction`/`TextEdit`.
//!
//! ## What is fixed
//!
//! One family of fix per diagnostic code, always **conservative** — an edit only ever rewrites the
//! flagged token or indentation, never anything that would change meaning beyond the correction:
//!
//! * [`TAB_INDENTATION`](crate::diagnostics::TAB_INDENTATION) → convert the flagged line's leading
//!   whitespace to spaces (each leading `\t` → 2 spaces, i.e. one indent level; a leading space stays
//!   one space).
//! * [`ODD_INDENTATION`](crate::diagnostics::ODD_INDENTATION) → round the leading spaces down to
//!   `2 * (sp / 2)` — the exact depth the engine derives (`depth = sp / 2`), so the fix agrees with
//!   the parser rather than guessing.
//! * [`UNKNOWN_PROPERTY`](crate::diagnostics::UNKNOWN_PROPERTY) → up to three "did you mean
//!   `<closest>`?" suggestions from [`catalog::PROPERTIES`](crate::catalog::PROPERTIES).
//! * [`UNKNOWN_STATE`](crate::diagnostics::UNKNOWN_STATE) → nearest [`schema::STATES`](crate::schema::STATES).
//! * [`INVALID_ANCHOR_EDGE`](crate::diagnostics::INVALID_ANCHOR_EDGE) → nearest
//!   [`schema::ANCHOR_EDGES`](crate::schema::ANCHOR_EDGES) or shorthand anchor.
//! * [`INVALID_PROPERTY_VALUE`](crate::diagnostics::INVALID_PROPERTY_VALUE) → nearest
//!   [`DISPLAY_VALUES`](crate::schema::DISPLAY_VALUES) / [`LAYOUT_TYPES`](crate::schema::LAYOUT_TYPES)
//!   for a `display`/`layout` value. `border` / `border-color*` values are **not** suggested: a
//!   `border` shorthand is a composite of width + style + color and a color literal has no small,
//!   safe nearest spelling, so any single-token replacement would risk changing meaning.
//!
//! A suggestion is only offered when a candidate is genuinely close — Levenshtein distance within
//! `max(2, word.len() / 3)` — and a suggestion is never offered when nothing is close enough (a
//! misleading fix is worse than none). Every list is deterministic: best (smallest distance) first,
//! ties broken by the candidate's order in its source set.
//!
//! No I/O, no `lsp-types`; byte offsets throughout.

use crate::{catalog, diagnostics, schema};
use lang_api::{ByteSpan, Diagnostic};

/// A single protocol-agnostic quick-fix: a titled set of byte-span replacements that corrects one
/// diagnostic.
///
/// Each entry in [`edits`](Self::edits) is a `(span, replacement)` pair — the half-open byte span to
/// replace and the text to put there. [`fixes_code`](Self::fixes_code) is the
/// [`diagnostics`](crate::diagnostics) code of the finding this fix addresses, so the server can link
/// the resulting `CodeAction` to the matching client diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// Human-readable action title (e.g. `Did you mean `width`?`).
    pub title: String,
    /// The replacements to apply, as `(span, replacement)` pairs. Spans are byte offsets into the
    /// same `source` passed to [`quick_fixes`]; replacements may be empty (a pure deletion).
    pub edits: Vec<(ByteSpan, String)>,
    /// The diagnostic code this fix corrects.
    pub fixes_code: &'static str,
}

/// Compute the quick-fixes offered for the byte `range` in `source` (spec §7).
///
/// The diagnostics are recomputed internally via [`diagnostics::analyze`]; only those whose span
/// overlaps `range` contribute a fix, and only the fixable codes yield one (a `syntax-error` or an
/// `invalid-indentation-depth`, for instance, has no mechanical correction and is skipped). The
/// result is deterministic and ordered by the underlying diagnostics' span order, with each
/// diagnostic's own suggestions ordered best-first.
///
/// `range` may be empty (a zero-width cursor position): overlap is inclusive of the boundaries, so a
/// fix is still offered for a diagnostic the cursor merely touches.
#[must_use]
pub fn quick_fixes(source: &str, range: ByteSpan) -> Vec<Fix> {
    let mut out = Vec::new();
    for diag in diagnostics::analyze(source) {
        if !overlaps(diag.span, range) {
            continue;
        }
        fixes_for(source, &diag, &mut out);
    }
    out
}

/// Whether two half-open byte spans touch or overlap. Boundary-inclusive so a zero-width cursor
/// range at the very start or end of a diagnostic still counts, matching editor expectations for
/// "the caret is on this squiggle".
fn overlaps(a: ByteSpan, b: ByteSpan) -> bool {
    a.start <= b.end && b.start <= a.end
}

/// Append the fix(es) for a single diagnostic to `out`, dispatching on its code.
fn fixes_for(source: &str, diag: &Diagnostic, out: &mut Vec<Fix>) {
    match diag.code {
        diagnostics::TAB_INDENTATION => out.push(tab_fix(source, diag)),
        diagnostics::ODD_INDENTATION => out.push(odd_fix(diag)),
        diagnostics::UNKNOWN_PROPERTY => {
            suggestion_fixes(source, diag, catalog::PROPERTIES, 3, out);
        }
        diagnostics::UNKNOWN_STATE => {
            suggestion_fixes(source, diag, schema::STATES, 1, out);
        }
        diagnostics::INVALID_ANCHOR_EDGE => {
            suggestion_fixes(source, diag, ANCHOR_CANDIDATES, 1, out);
        }
        diagnostics::INVALID_PROPERTY_VALUE => {
            if let Some(set) = value_candidate_set(&diag.message) {
                suggestion_fixes(source, diag, set, 1, out);
            }
        }
        _ => {}
    }
}

/// The candidate anchor spellings for an invalid-anchor-edge suggestion: the six edges plus the two
/// shorthand keys (`fill` / `centerIn`), since either may sit in the flagged edge position.
const ANCHOR_CANDIDATES: &[&str] = &[
    "top",
    "bottom",
    "left",
    "right",
    "horizontalCenter",
    "verticalCenter",
    "fill",
    "centerIn",
];

/// Pick the candidate set for an [`INVALID_PROPERTY_VALUE`](crate::diagnostics::INVALID_PROPERTY_VALUE)
/// suggestion from the diagnostic message, which names the offending property family. Only `display`
/// and `layout` have a small closed set of valid spellings to suggest from; `border` (a composite
/// width + style + color) and `border-color*` (an arbitrary color literal) are deliberately skipped —
/// there is no single-token nearest spelling that is safe to offer.
fn value_candidate_set(message: &str) -> Option<&'static [&'static str]> {
    if message.contains("`display`") {
        Some(schema::DISPLAY_VALUES)
    } else if message.contains("`layout`") {
        Some(schema::LAYOUT_TYPES)
    } else {
        None
    }
}

/// Build up to `max` "did you mean `<candidate>`?" fixes for the token the diagnostic spans, drawn
/// from `candidates` and ordered best-first. The flagged token's text is `source[diag.span]`; a
/// candidate is offered only when it is genuinely close (see [`closest`]).
fn suggestion_fixes(
    source: &str,
    diag: &Diagnostic,
    candidates: &[&str],
    max: usize,
    out: &mut Vec<Fix>,
) {
    let word = &source[diag.span.start..diag.span.end];
    for candidate in closest(word, candidates, max) {
        out.push(Fix {
            title: format!("Did you mean `{candidate}`?"),
            edits: vec![(diag.span, candidate.to_owned())],
            fixes_code: diag.code,
        });
    }
}

/// The up-to-`max` closest `candidates` to `word`, best (smallest edit distance) first, ties broken
/// by the candidate's position in `candidates` (a stable sort preserves that source order).
///
/// A candidate is a match only when its [`edit_distance`] to `word` is within `max(2, word.len() /
/// 3)` and greater than zero (an exact match is never "did you mean"). The threshold is deliberately
/// tight: for short tokens it is the constant 2, loosening to ~a third of the length only for longer
/// tokens where more typos are plausible. When nothing is within the threshold the list is empty —
/// the caller then offers no fix, preferring silence to a misleading suggestion.
fn closest(word: &str, candidates: &[&str], max: usize) -> Vec<String> {
    let threshold = (word.len() / 3).max(2);
    let mut scored: Vec<(usize, usize, &str)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, &cand)| {
            let d = edit_distance(word, cand);
            (d > 0 && d <= threshold).then_some((d, i, cand))
        })
        .collect();
    scored.sort_by_key(|&(d, i, _)| (d, i));
    scored
        .into_iter()
        .take(max)
        .map(|(_, _, cand)| cand.to_owned())
        .collect()
}

/// The edit distance between two strings, over Unicode scalar values: the Damerau-Levenshtein
/// *optimal string alignment* distance — Levenshtein's insert/delete/substitute plus a unit cost for
/// swapping two **adjacent** characters. A full-matrix dynamic program: O(a·b) time and space, pure
/// and total.
///
/// The transposition rule is what makes it a good "did you mean" metric: a finger-slip typo like
/// `widht` for `width` is one swap (distance 1), clearly closer than an unrelated word two
/// substitutions away — plain Levenshtein would score both at 2 and tie them.
#[must_use]
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    // `d[i][j]` = distance between a[..i] and b[..j].
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (d[i - 1][j] + 1) // deletion
                .min(d[i][j - 1] + 1) // insertion
                .min(d[i - 1][j - 1] + cost); // substitution
            // Adjacent transposition (`ab` <-> `ba`).
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// Build the tabs→spaces fix for a `tab-indentation` diagnostic: replace the flagged line's whole
/// leading-whitespace run with spaces, mapping each leading `\t` to 2 spaces (one indent level) and
/// keeping each leading space as-is. The diagnostic spans a single tab byte, so the line start is
/// recovered from it.
fn tab_fix(source: &str, diag: &Diagnostic) -> Fix {
    let line_start = line_start_of(source, diag.span.start);
    let ws_end = leading_whitespace_end(source, line_start);
    let mut replacement = String::new();
    for &byte in &source.as_bytes()[line_start..ws_end] {
        match byte {
            b'\t' => replacement.push_str("  "),
            _ => replacement.push(' '),
        }
    }
    Fix {
        title: "Convert tabs to spaces".to_owned(),
        edits: vec![(ByteSpan::new(line_start, ws_end), replacement)],
        fixes_code: diag.code,
    }
}

/// Build the "fix indentation" fix for an `odd-indentation` diagnostic: replace the flagged leading
/// spaces (exactly the diagnostic's span) with `2 * (sp / 2)` spaces — the even indent for the depth
/// the engine derives (`depth = sp / 2`), so the corrected indentation agrees with the parser.
fn odd_fix(diag: &Diagnostic) -> Fix {
    // The diagnostic spans exactly the odd leading-space run; `sp` is its length.
    let sp = diag.span.len();
    let target = 2 * (sp / 2);
    Fix {
        title: format!("Fix indentation to {target} spaces"),
        edits: vec![(diag.span, " ".repeat(target))],
        fixes_code: diag.code,
    }
}

/// The byte offset of the start of the line containing `offset` (the byte just after the preceding
/// `\n`, or 0 for the first line).
fn line_start_of(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |nl| nl + 1)
}

/// The byte offset just past the leading run of spaces and tabs starting at `line_start`.
fn leading_whitespace_end(source: &str, line_start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut end = line_start;
    while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src`.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// A range covering the whole document.
    fn whole(src: &str) -> ByteSpan {
        ByteSpan::new(0, src.len())
    }

    fn titles(fixes: &[Fix]) -> Vec<&str> {
        fixes.iter().map(|f| f.title.as_str()).collect()
    }

    // --- edit-distance helper -----------------------------------------------------------------

    #[test]
    fn edit_distance_known_pairs() {
        assert_eq!(edit_distance("widht", "width"), 1); // one adjacent transposition
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("hoer", "hover"), 1); // one insertion
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("flaw", "lawn"), 2);
    }

    // --- tab-indentation ----------------------------------------------------------------------

    #[test]
    fn tab_indentation_yields_tabs_to_spaces_fix() {
        // A single leading tab becomes two spaces (one indent level).
        let src = "Panel\n\tid: main\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Convert tabs to spaces"]);
        let (span, repl) = &fixes[0].edits[0];
        // The edit replaces the leading `\t` (byte 6..7) with two spaces.
        assert_eq!(*span, ByteSpan::new(6, 7));
        assert_eq!(repl, "  ");
        assert_eq!(fixes[0].fixes_code, diagnostics::TAB_INDENTATION);
    }

    #[test]
    fn tab_after_spaces_converts_the_whole_leading_run() {
        // Two spaces then a tab: the whole leading run is rewritten to `  ` + `  ` = four spaces.
        let src = "Panel\n  \tid: x\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Convert tabs to spaces"]);
        let (span, repl) = &fixes[0].edits[0];
        // Leading run is bytes 6..9 ("  \t").
        assert_eq!(*span, ByteSpan::new(6, 9));
        assert_eq!(repl, "    ");
    }

    // --- odd-indentation ----------------------------------------------------------------------

    #[test]
    fn odd_indentation_rounds_to_engine_depth() {
        // Three leading spaces (depth 1) round down to two spaces.
        let src = "Panel\n  id: x\n   size: y\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Fix indentation to 2 spaces"]);
        let (span, repl) = &fixes[0].edits[0];
        // The three spaces before `size` are the flagged span.
        let start = at(src, "   size");
        assert_eq!(*span, ByteSpan::new(start, start + 3));
        assert_eq!(repl, "  ");
        assert_eq!(fixes[0].fixes_code, diagnostics::ODD_INDENTATION);
    }

    #[test]
    fn one_space_indentation_rounds_to_zero() {
        let src = "Panel\n id: x\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Fix indentation to 0 spaces"]);
        assert_eq!(fixes[0].edits[0].1, "");
    }

    // --- unknown-property ---------------------------------------------------------------------

    #[test]
    fn unknown_property_suggests_the_closest_catalog_entry() {
        // `colr` has exactly one catalog entry within the threshold: `color`.
        let src = "Panel\n  colr: red\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Did you mean `color`?"]);
        let (span, repl) = &fixes[0].edits[0];
        let start = at(src, "colr");
        assert_eq!(*span, ByteSpan::new(start, start + 4));
        assert_eq!(repl, "color");
        assert_eq!(fixes[0].fixes_code, diagnostics::UNKNOWN_PROPERTY);
    }

    #[test]
    fn widht_suggests_width_first() {
        // The DoD's canonical typo: `widht` → `width` is the best (single-transposition) suggestion.
        let src = "Panel\n  widht: 10\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(fixes[0].title, "Did you mean `width`?");
        assert_eq!(fixes[0].edits[0].1, "width");
        assert!(
            fixes
                .iter()
                .all(|f| f.fixes_code == diagnostics::UNKNOWN_PROPERTY),
            "{:?}",
            titles(&fixes)
        );
    }

    #[test]
    fn nonsense_property_gets_no_suggestion() {
        // Nothing in the catalog is within the edit-distance threshold of this token.
        let src = "Panel\n  zzzzzzzz: 1\n";
        let fixes = quick_fixes(src, whole(src));
        assert!(fixes.is_empty(), "expected no fix, got {fixes:?}");
    }

    #[test]
    fn unknown_property_offers_at_most_three_suggestions() {
        // `widt` is close to several `width`-family tags; at most three come back, best-first.
        let src = "Panel\n  widt: 1\n";
        let fixes = quick_fixes(src, whole(src));
        assert!(fixes.len() <= 3, "at most three: {:?}", titles(&fixes));
        assert!(
            fixes
                .iter()
                .all(|f| f.fixes_code == diagnostics::UNKNOWN_PROPERTY),
            "{:?}",
            titles(&fixes)
        );
        // The single-edit `width` must be the first, best suggestion.
        assert_eq!(fixes[0].title, "Did you mean `width`?");
    }

    // --- unknown-state ------------------------------------------------------------------------

    #[test]
    fn unknown_state_suggests_the_closest_state() {
        // `$hoer` is one edit from `hover`.
        let src = "Button\n  $hoer:\n    color: red\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Did you mean `hover`?"]);
        let (span, repl) = &fixes[0].edits[0];
        let start = at(src, "hoer");
        assert_eq!(*span, ByteSpan::new(start, start + 4));
        assert_eq!(repl, "hover");
        assert_eq!(fixes[0].fixes_code, diagnostics::UNKNOWN_STATE);
    }

    // --- invalid-anchor-edge ------------------------------------------------------------------

    #[test]
    fn invalid_anchor_edge_suggests_the_nearest_edge() {
        // `topp` is one edit from the `top` edge.
        let src = "Widget\n  anchors.topp: parent.top\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Did you mean `top`?"]);
        let (span, repl) = &fixes[0].edits[0];
        let start = at(src, "topp");
        assert_eq!(*span, ByteSpan::new(start, start + 4));
        assert_eq!(repl, "top");
        assert_eq!(fixes[0].fixes_code, diagnostics::INVALID_ANCHOR_EDGE);
    }

    // --- invalid-property-value ---------------------------------------------------------------

    #[test]
    fn invalid_display_value_suggests_the_nearest_display() {
        let src = "Panel\n  display: blocky\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Did you mean `block`?"]);
        let (span, repl) = &fixes[0].edits[0];
        let start = at(src, "blocky");
        assert_eq!(*span, ByteSpan::new(start, start + 6));
        assert_eq!(repl, "block");
        assert_eq!(fixes[0].fixes_code, diagnostics::INVALID_PROPERTY_VALUE);
    }

    #[test]
    fn invalid_layout_value_suggests_the_nearest_layout() {
        // Mis-cased `verticalbox` is one edit from `verticalBox`.
        let src = "Panel\n  layout: verticalbox\n";
        let fixes = quick_fixes(src, whole(src));
        assert_eq!(titles(&fixes), vec!["Did you mean `verticalBox`?"]);
        assert_eq!(fixes[0].edits[0].1, "verticalBox");
    }

    #[test]
    fn invalid_border_value_gets_no_suggestion() {
        // `border` is a composite value we deliberately do not auto-fix.
        let src = "Panel\n  border: red\n";
        let fixes = quick_fixes(src, whole(src));
        assert!(fixes.is_empty(), "border must not be suggested: {fixes:?}");
    }

    // --- range filtering ----------------------------------------------------------------------

    #[test]
    fn only_diagnostics_overlapping_the_range_yield_fixes() {
        // Two unknown properties on different lines; a range over just the second offers only its fix.
        let src = "Panel\n  hieght: 1\n  colr: red\n";
        let colr = at(src, "colr");
        let range = ByteSpan::new(colr, colr + 4);
        let fixes = quick_fixes(src, range);
        assert_eq!(titles(&fixes), vec!["Did you mean `color`?"]);
    }

    #[test]
    fn a_range_over_the_first_diagnostic_excludes_the_second() {
        let src = "Panel\n  hieght: 1\n  colr: red\n";
        let hieght = at(src, "hieght");
        let range = ByteSpan::new(hieght, hieght + 6);
        let fixes = quick_fixes(src, range);
        // `hieght` → `height` (a single transposition) leads; crucially the out-of-range `colr`
        // line contributes nothing.
        assert_eq!(fixes[0].title, "Did you mean `height`?");
        assert!(
            !titles(&fixes).contains(&"Did you mean `color`?"),
            "out-of-range diagnostic leaked in: {:?}",
            titles(&fixes)
        );
    }

    #[test]
    fn a_zero_width_cursor_touching_a_diagnostic_still_offers_the_fix() {
        // Caret exactly at the start of the flagged token (an empty range) still overlaps it.
        let src = "Panel\n  colr: red\n";
        let colr = at(src, "colr");
        let fixes = quick_fixes(src, ByteSpan::new(colr, colr));
        assert_eq!(titles(&fixes), vec!["Did you mean `color`?"]);
    }

    #[test]
    fn unfixable_codes_produce_no_fix() {
        // A structural syntax error has no mechanical correction.
        let src = "x: [a, b\n";
        let fixes = quick_fixes(src, whole(src));
        assert!(fixes.is_empty(), "syntax errors are not fixable: {fixes:?}");
    }
}
