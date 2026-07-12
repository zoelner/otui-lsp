//! Indexing where OTClient **Lua** code refers to a widget's `id:` (spec §2.3), the foundation of
//! the OTUI↔Lua bridge: find-references from an `id:` declaration, and (later) go-to-definition
//! from Lua back into the `.otui` tree.
//!
//! Spec §2.3 names three cross-reference forms:
//!
//! ```lua
//! widget:getChildById('closeButton')            -- form 1
//! widget:recursiveGetChildById('closeButton')   -- form 2
//! controller.ui.closeButton:setText('x')        -- form 3: a dot-chain segment after `.ui.`
//! ```
//!
//! plus a fourth, def-side form: a widget created at runtime in Lua can be given an id that never
//! appears in any `.otui` file:
//!
//! ```lua
//! button:setId("bidButton")
//! ```
//!
//! [`scan_id_refs`] finds the first three (as [`LuaIdRef`]s); [`scan_id_defs`] finds the fourth (as
//! [`LuaIdDef`]s). [`LuaRefIndex`] aggregates refs across the workspace, mirroring
//! [`StyleIndex`](crate::style_index::StyleIndex)'s API.
//!
//! ## Corpus-derived rules — this is what shapes the scan
//!
//! Measured against the full OTClient engine tree (375 `.lua` / 205 `.otui`):
//!
//! * **Only a string literal that is the COMPLETE argument counts.** `getChildById('perkColumn_'
//!   .. i)` builds the id at runtime by concatenation (85 such calls in the engine) — it can never
//!   be navigated or diagnosed, so it must never be indexed. A call only yields a ref when its
//!   argument list is *exactly* one quoted literal: optional whitespace, the opening quote, the
//!   literal body, the closing quote, optional whitespace, `)`. Anything else — a bare variable, a
//!   concatenation, a second argument — yields nothing for that call.
//! * **`.ui.<name>` is ambiguous.** It is used both for widget-id access (`controller.ui.closeBtn`)
//!   and as a plain Lua field on a controller's own state table (e.g. `controller.ui.moveOnlyToMain
//!   = not extendedView` is not a widget access at all). This module does not — and cannot,
//!   text-only — disambiguate the two: every dot-chain segment after `.ui.` is recorded, tagged
//!   [`LuaIdRefKind::DotUi`], and it is the **consumer's** job to decide whether it matches a known
//!   id (no match ⇒ no navigation, no diagnostic — silence, not noise).
//! * **`setId("literal")` matters too.** Some references in the engine resolve to a widget created
//!   purely at runtime (`button:setId("bidButton")`), never declared in any `.otui` — Lua is the id's
//!   only definition site. Indexing the literal form of `setId` gives navigation for free; the 179
//!   `setId(<non-literal>)` calls are excluded by the same complete-literal rule, since an id
//!   assembled at run time has no source location to navigate to.
//!
//! ## Measured on the engine (370 readable `.lua` of 375; 5 are not valid UTF-8)
//!
//! | | |
//! |---|---|
//! | `getChildById` | 800 |
//! | `recursiveGetChildById` | 571 |
//! | `.ui.` chain segments | 493 |
//! | `setId("literal")` defs | 45 |
//! | concatenation calls indexed | 0 |
//!
//! These were re-derived **after** the exclusion pre-pass was fixed, and cross-checked against an
//! independently written scanner. The earlier figures in this module were the output of a buggy
//! pre-pass validating itself — a number a scanner reports about its own corpus proves nothing until
//! something else counts it too.
//!
//! ## Heuristic parse (no Lua grammar)
//!
//! Exactly like [`lua_widgets`](crate::lua_widgets): there is no Lua parser in this workspace, so
//! the scan is byte-oriented and deliberately conservative. Unlike `lua_widgets`, it *does* need an
//! excluded-range pre-pass — a `getChildById('x')` written inside a comment or a string must never
//! become a clickable reference. That pre-pass ([`excluded_ranges`]) covers `--` line comments, long
//! comments and long strings at **any** bracket level (`--[==[ … ]==]`, `[==[ … ]==]`), and
//! short-string bodies with correct backslash-escape handling.
//!
//! Two of those were learned the hard way, and both are load-bearing rather than pedantic:
//!
//! * **Escapes must be scanned forward, not sniffed backwards.** Testing "is the byte before this
//!   quote a backslash?" misreads a string ending in an escaped backslash — `'\\'`, as in the real
//!   `path:gsub('\\', '/')`. The closing quote looks escaped, the scan runs to the next quote in the
//!   file, and quote parity flips for everything below: references simply stop being found, silently.
//! * **A short string ends at the newline.** Lua forbids a raw newline inside one, and an LSP buffer
//!   is mid-edit most of the time — so a half-typed quote is the normal case. Bounding it to its own
//!   line is what keeps one stray character from blanking the rest of the file.

use crate::style_index::DocId;
use lang_api::ByteSpan;
use std::collections::HashMap;

/// Which of the three spec §2.3 reference forms a [`LuaIdRef`] was found as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaIdRefKind {
    /// `widget:getChildById('id')` — a single, direct child lookup.
    GetChildById,
    /// `widget:recursiveGetChildById('id')` — a lookup over the whole subtree.
    RecursiveGetChildById,
    /// A dot-chain segment following `.ui.`, e.g. the `closeButton` in
    /// `controller.ui.closeButton:setText('x')`. Best-effort: this form is also used for plain Lua
    /// controller state (spec-corpus rule above), so a `DotUi` ref is not guaranteed to name a real
    /// widget id.
    DotUi,
}

/// One place in a Lua source that refers to a widget `id:` value.
///
/// `span` covers the **id token itself** — the text inside the quotes for the two `getChildById`
/// forms, or the identifier segment for a `DotUi` chain — not the surrounding call/expression, so a
/// consumer can turn it directly into a `Location` that lands the cursor on the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaIdRef {
    pub id: String,
    pub span: ByteSpan,
    pub kind: LuaIdRefKind,
}

/// One place in a Lua source that **defines** a widget id at runtime via `setId("literal")`.
///
/// `span` covers just the literal's inner text, like [`LuaIdRef::span`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaIdDef {
    pub id: String,
    pub span: ByteSpan,
}

/// Find every `getChildById`/`recursiveGetChildById` call whose sole argument is a complete string
/// literal, plus every dot-chain segment following a `.ui.` member access, in `source` (spec §2.3).
///
/// Comments (`--` and `--[[ ]]`) and the contents of unrelated string literals are excluded from
/// consideration before either form is scanned for, so a reference-shaped call written inside a
/// comment is never indexed. The returned refs are ordered by their span's start offset.
#[must_use]
pub fn scan_id_refs(source: &str) -> Vec<LuaIdRef> {
    let excluded = excluded_ranges(source);
    let mut out: Vec<LuaIdRef> = call_string_literals(source, &excluded, "getChildById")
        .map(|(id, span)| LuaIdRef {
            id,
            span,
            kind: LuaIdRefKind::GetChildById,
        })
        .chain(
            call_string_literals(source, &excluded, "recursiveGetChildById").map(|(id, span)| {
                LuaIdRef {
                    id,
                    span,
                    kind: LuaIdRefKind::RecursiveGetChildById,
                }
            }),
        )
        .collect();
    collect_dot_ui_refs(source, &excluded, &mut out);
    out.sort_by_key(|r| r.span.start);
    out
}

/// Find every `setId("literal")` call in `source` whose argument is a complete string literal
/// (spec §2.3 corpus rule: `setId(<variable>)` is excluded by construction). Comments are excluded
/// the same way as in [`scan_id_refs`]. The returned defs are ordered by their span's start offset.
#[must_use]
pub fn scan_id_defs(source: &str) -> Vec<LuaIdDef> {
    let excluded = excluded_ranges(source);
    let mut out: Vec<LuaIdDef> = call_string_literals(source, &excluded, "setId")
        .map(|(id, span)| LuaIdDef { id, span })
        .collect();
    out.sort_by_key(|d| d.span.start);
    out
}

/// Every whole-word call to `name` in `source` whose sole argument is a complete string literal, as
/// `(literal, content_span)`. Shared by the `getChildById`/`recursiveGetChildById` ref forms and the
/// `setId` def form — all three are "a call whose one argument is a bare literal". A call whose name
/// occurs inside a comment or string (per `excluded`), whose argument is not a lone literal (a
/// variable, a concatenation, more than one argument), or whose literal is empty, contributes
/// nothing.
fn call_string_literals<'a>(
    source: &'a str,
    excluded: &'a [(usize, usize)],
    name: &'a str,
) -> impl Iterator<Item = (String, ByteSpan)> + 'a {
    source.match_indices(name).filter_map(move |(idx, _)| {
        if !is_ident_boundary_before(source, idx)
            || !is_ident_boundary_after(source, idx + name.len())
        {
            return None;
        }
        if in_excluded(excluded, idx) {
            return None;
        }
        let after_name = &source[idx + name.len()..];
        let after_ws = after_name.trim_start();
        after_ws.strip_prefix('(')?;
        let paren_pos = idx + name.len() + (after_name.len() - after_ws.len());
        let args_start = paren_pos + 1;
        let (literal, rel_start, rel_end) = sole_string_literal_arg(&source[args_start..])?;
        if literal.is_empty() {
            return None;
        }
        Some((
            literal,
            ByteSpan::new(args_start + rel_start, args_start + rel_end),
        ))
    })
}

/// Parse a call's sole, complete string-literal argument from `rest` — the text immediately
/// following the call's opening `(`. Returns `(literal_text, rel_start, rel_end)`, the decoded
/// literal and the byte offsets of its **content** (excluding the quotes) relative to `rest`, only
/// when `rest` is: optional whitespace, a quoted literal, optional whitespace, `)`.
///
/// Anything else — a bare identifier, a concatenation (`'x' .. y`), a second argument, an unclosed
/// literal — yields `None`: the id is not known at scan time, so it cannot be indexed. This is the
/// mechanism behind the corpus rule that a concatenation-built id (`'perkColumn_' .. i`) is never
/// picked up — the text after the closing quote is `.. i)`, not (after trimming) `)`, so the match
/// fails here. No escape handling (consistent with the rest of this crate's Lua-as-text scanning):
/// a literal's content runs to the next occurrence of its own quote character.
fn sole_string_literal_arg(rest: &str) -> Option<(String, usize, usize)> {
    let ws = rest.len() - rest.trim_start().len();
    let quote = rest[ws..].chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let content_start = ws + quote.len_utf8();
    let close_rel = rest[content_start..].find(quote)? + content_start;
    let after = rest[close_rel + quote.len_utf8()..].trim_start();
    if after.starts_with(')') {
        Some((
            rest[content_start..close_rel].to_owned(),
            content_start,
            close_rel,
        ))
    } else {
        None
    }
}

/// Collect every `.ui.<ident>(.<ident>)*` dot-chain reference in `source` into `out`.
///
/// Spec §2.3: "`controller.ui.someId.childId` — every identifier after `.ui.` is an `id:` value" —
/// so each segment of the chain (not just the first) is pushed as its own [`LuaIdRef`]. A `.ui.`
/// match only starts a chain when it is itself a member access (the byte before the leading `.` is
/// an identifier character, e.g. the `r` of `controller`); a bare `.ui.` with nothing before it (or
/// found inside a comment/string) is not a reference. The chain stops at the first non-identifier
/// continuation — a method call (`:`), an index (`(`/`[`), or plain whitespace — so
/// `...ui.dailyRewardsPanel:getChildById(...)` stops at `dailyRewardsPanel`.
fn collect_dot_ui_refs(source: &str, excluded: &[(usize, usize)], out: &mut Vec<LuaIdRef>) {
    const PATTERN: &str = ".ui.";
    let mut search = 0;
    while let Some(rel) = source[search..].find(PATTERN) {
        let dot_pos = search + rel;
        search = dot_pos + 1;

        if dot_pos == 0 || !is_ident_byte(source.as_bytes()[dot_pos - 1]) {
            continue;
        }
        if in_excluded(excluded, dot_pos) {
            continue;
        }

        let mut pos = dot_pos + PATTERN.len();
        loop {
            let seg_start = pos;
            let seg_end = ident_end(source, seg_start);
            if seg_end == seg_start {
                break;
            }
            out.push(LuaIdRef {
                id: source[seg_start..seg_end].to_owned(),
                span: ByteSpan::new(seg_start, seg_end),
                kind: LuaIdRefKind::DotUi,
            });
            if source.as_bytes().get(seg_end) == Some(&b'.') && is_ident_start(source, seg_end + 1)
            {
                pos = seg_end + 1;
            } else {
                break;
            }
        }
    }
}

/// Byte ranges of `source` that must not be treated as Lua code when locating a call name or a
/// `.ui.` chain: `--` line comments, `--[[ ... ]]` block comments, and single/double-quoted string
/// literal bodies (so a reference-shaped snippet mentioned inside an unrelated string is not
/// mistaken for a real one either). Sorted and non-overlapping, half-open `[start, end)`.
fn excluded_ranges(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut ranges = Vec::new();
    let mut i = 0usize;
    while i < len {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                let start = i;
                let mut j = i + 1;
                let end = loop {
                    let Some(&b) = bytes.get(j) else { break len };
                    match b {
                        // An escape consumes whatever byte follows it. Testing the *preceding* byte
                        // instead (`bytes[j - 1] != b'\\'`) misreads a string that ends in an escaped
                        // backslash — `'\\'`, a single literal backslash, as in the real
                        // `path:gsub('\\', '/')`. There the closing quote is preceded by `\` and so
                        // looks escaped; the scan then runs on to the next quote in the file,
                        // swallowing a real opening quote and flipping quote parity for the entire
                        // remainder of the source. Everything after it silently stops being indexed.
                        b'\\' => j += 2,
                        // A Lua short string cannot span a raw newline. Stopping here bounds the blast
                        // radius of an unterminated quote — routine in a buffer that is being typed —
                        // to a single line instead of the rest of the file.
                        b'\n' => break j,
                        b if b == quote => break j + 1,
                        _ => j += 1,
                    }
                };
                ranges.push((start, end));
                i = end;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                let start = i;
                // A long comment at any bracket level: `--[[ … ]]`, `--[==[ … ]==]`. Matching only
                // the literal `--[[` would leave the body of a level-N comment to be scanned as
                // code, handing back a clickable reference that points inside a comment.
                if let Some(end) = long_bracket_end(source, i + 2) {
                    ranges.push((start, end));
                    i = end;
                } else {
                    let end = source[i..].find('\n').map_or(len, |rel| i + rel);
                    ranges.push((start, end));
                    i = end;
                }
            }
            // A long-bracket *string*: `[[ … ]]`, `[=[ … ]=]`. Its body is opaque and may hold
            // anything — the engine has 138 of these, some embedding literal OTUI source, and three
            // with an odd number of quotes in the body, which would desync the short-string scanner
            // above. Skipping it wholesale is what keeps both a bogus reference and that desync out.
            b'[' => {
                if let Some(end) = long_bracket_end(source, i) {
                    ranges.push((i, end));
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    ranges
}

/// The byte offset just past the close of the Lua long bracket opening at `at` — `[`, then N `=`,
/// then `[`, closed by `]`, N `=`, `]` — or the end of `source` when it is never closed. [`None`] when
/// no long bracket opens at `at`.
///
/// One matcher serves both long comments (`--[==[ … ]==]`) and long strings (`[==[ … ]==]`); they
/// differ only in the `--` in front.
fn long_bracket_end(source: &str, at: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(at) != Some(&b'[') {
        return None;
    }
    let mut level = 0usize;
    let mut k = at + 1;
    while bytes.get(k) == Some(&b'=') {
        level += 1;
        k += 1;
    }
    if bytes.get(k) != Some(&b'[') {
        return None;
    }
    let body = k + 1;
    let mut close = String::with_capacity(level + 2);
    close.push(']');
    close.extend(std::iter::repeat('=').take(level));
    close.push(']');
    Some(
        source[body..]
            .find(&close)
            .map_or(source.len(), |rel| body + rel + close.len()),
    )
}

/// Whether byte offset `pos` falls inside one of the sorted, non-overlapping `ranges`.
fn in_excluded(ranges: &[(usize, usize)], pos: usize) -> bool {
    ranges
        .binary_search_by(|&(start, end)| {
            if pos < start {
                std::cmp::Ordering::Greater
            } else if pos >= end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Whether the byte immediately before `idx` (if any) is not an identifier character — i.e. `idx`
/// starts a whole word.
fn is_ident_boundary_before(source: &str, idx: usize) -> bool {
    idx == 0 || !is_ident_byte(source.as_bytes()[idx - 1])
}

/// Whether the byte at `idx` (if any) is not an identifier character — i.e. the word ending at `idx`
/// is whole.
fn is_ident_boundary_after(source: &str, idx: usize) -> bool {
    // `map_or(true, …)` rather than `Option::is_none_or` — the latter is only stable since Rust
    // 1.82, but the workspace MSRV is 1.75.
    source
        .as_bytes()
        .get(idx)
        .map_or(true, |&b| !is_ident_byte(b))
}

/// The end offset of the identifier run starting at `start` (may equal `start` if none).
fn ident_end(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && is_ident_byte(bytes[i]) {
        i += 1;
    }
    i
}

/// Whether `source` has an identifier-start character (`[A-Za-z_]`) at byte offset `idx`.
fn is_ident_start(source: &str, idx: usize) -> bool {
    source
        .as_bytes()
        .get(idx)
        .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_')
}

/// Whether `b` may appear inside a Lua identifier.
fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The workspace-wide index of Lua `id:` references, aggregated per document.
///
/// Mirrors [`StyleIndex`](crate::style_index::StyleIndex) and
/// [`LuaWidgetIndex`](crate::lua_widgets::LuaWidgetIndex): each Lua document contributes the
/// [`LuaIdRef`]s [`scan_id_refs`] found in it, keyed by an opaque [`DocId`]; the server re-indexes
/// one document at a time ([`set_document`](Self::set_document)) or drops it
/// ([`remove_document`](Self::remove_document)) as files change. [`lookup`](Self::lookup) fans out
/// across every document, since an id can be referenced from any Lua module in the workspace.
#[derive(Debug, Default)]
pub struct LuaRefIndex {
    by_doc: HashMap<DocId, Vec<LuaIdRef>>,
}

impl LuaRefIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace **all** id refs for one document (re-index on change).
    pub fn set_document(&mut self, doc: impl Into<DocId>, refs: Vec<LuaIdRef>) {
        self.by_doc.insert(doc.into(), refs);
    }

    /// Remove a document and its refs (e.g. on delete), returning them if present.
    pub fn remove_document(&mut self, doc: &DocId) -> Option<Vec<LuaIdRef>> {
        self.by_doc.remove(doc)
    }

    /// The refs a single document currently contributes, if it is indexed.
    #[must_use]
    pub fn document(&self, doc: &DocId) -> Option<&[LuaIdRef]> {
        self.by_doc.get(doc).map(Vec::as_slice)
    }

    /// Every ref naming the id `id` across **all** documents, paired with the document each was
    /// found in. Because `DotUi` refs are best-effort (spec-corpus rule), a match here is not a
    /// guarantee the id is real — it is the caller's job to cross-check against a known `id:`
    /// declaration before treating it as navigable.
    ///
    /// **This fans out across the whole workspace, and spec §5.4 does not.** An `id:` is
    /// document-local (ids repeat freely across modules — `holder` is declared in both
    /// `30-messageboxes.otui` and `game_store/style/ui.otui`), so find-references for a declaration
    /// is scoped to the **paired** `.lua` controller (§2.3: the sibling of the same stem). A consumer
    /// that hands this method's full result straight to the client will report cross-module
    /// references that are not references to that declaration at all. Use [`document`](Self::document)
    /// to scope to the paired file; `lookup` is the unscoped primitive underneath it.
    #[must_use]
    pub fn lookup(&self, id: &str) -> Vec<(&DocId, &LuaIdRef)> {
        self.iter().filter(|(_, r)| r.id == id).collect()
    }

    /// Iterate every `(document, ref)` pair in the index.
    pub fn iter(&self) -> impl Iterator<Item = (&DocId, &LuaIdRef)> {
        self.by_doc
            .iter()
            .flat_map(|(doc, refs)| refs.iter().map(move |r| (doc, r)))
    }

    /// The number of documents currently indexed.
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.by_doc.len()
    }

    /// Whether the index holds no documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_doc.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(src: &str, span: ByteSpan) -> &str {
        &src[span.start..span.end]
    }

    #[test]
    fn get_child_by_id_span_lands_on_the_id_token() {
        let src = "widget:getChildById('closeButton')\n";
        let refs = scan_id_refs(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, LuaIdRefKind::GetChildById);
        assert_eq!(refs[0].id, "closeButton");
        assert_eq!(text(src, refs[0].span), "closeButton");
    }

    #[test]
    fn recursive_get_child_by_id_span_lands_on_the_id_token() {
        let src = "widget:recursiveGetChildById('closeButton')\n";
        let refs = scan_id_refs(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, LuaIdRefKind::RecursiveGetChildById);
        assert_eq!(refs[0].id, "closeButton");
        assert_eq!(text(src, refs[0].span), "closeButton");
    }

    #[test]
    fn dot_ui_chain_segment_span_lands_on_the_identifier() {
        let src = "controller.ui.closeButton:setText('x')\n";
        let refs = scan_id_refs(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, LuaIdRefKind::DotUi);
        assert_eq!(refs[0].id, "closeButton");
        assert_eq!(text(src, refs[0].span), "closeButton");
    }

    #[test]
    fn dot_ui_chain_indexes_every_segment_after_ui() {
        // Spec §2.3: "every identifier after `.ui.` is an `id:` value" — not just the first.
        let src = "rewardWallController.ui.restingAreaPanel.restingAreaInfo.rewardStreakIcon:setText(x)\n";
        let refs = scan_id_refs(src);
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["restingAreaPanel", "restingAreaInfo", "rewardStreakIcon"]
        );
        assert!(refs.iter().all(|r| r.kind == LuaIdRefKind::DotUi));
    }

    #[test]
    fn dot_ui_chain_stops_at_a_method_call() {
        let src = "c.ui.dailyRewardsPanel:getChildById(\"reward\" .. index)\n";
        let refs = scan_id_refs(src);
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["dailyRewardsPanel"]);
    }

    #[test]
    fn both_quote_styles_are_recognized() {
        let single = scan_id_refs("widget:getChildById('single')\n");
        assert_eq!(single[0].id, "single");
        let double = scan_id_refs("widget:getChildById(\"double\")\n");
        assert_eq!(double[0].id, "double");
    }

    #[test]
    fn a_concatenated_argument_is_not_indexed() {
        // `getChildById('perkColumn_' .. i)` builds the id at runtime; it is not a compile-time
        // literal, so it can never be navigated or diagnosed — indexing it would be a false
        // positive (a "reference" that does not point anywhere real). The text right after the
        // closing quote is ` .. i)`, not `)`, so `sole_string_literal_arg` rejects the whole call.
        let src = "widget:getChildById('perkColumn_' .. i)\n";
        assert!(
            scan_id_refs(src).is_empty(),
            "a concatenation-built id must never be indexed"
        );
    }

    #[test]
    fn a_variable_argument_is_not_indexed() {
        let src = "widget:getChildById(someVariable)\n";
        assert!(scan_id_refs(src).is_empty());
    }

    #[test]
    fn set_id_with_a_literal_is_indexed_as_a_def() {
        let src = "button:setId(\"bidButton\")\n";
        let defs = scan_id_defs(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].id, "bidButton");
        assert_eq!(text(src, defs[0].span), "bidButton");
    }

    #[test]
    fn set_id_with_a_variable_is_not_indexed() {
        let src = "button:setId(data.id)\n";
        assert!(scan_id_defs(src).is_empty());
    }

    #[test]
    fn a_string_ending_in_an_escaped_backslash_does_not_swallow_the_rest_of_the_file() {
        // The bug this pins was real and silent. `bytes[j - 1] != b'\\'` treats the closing quote of
        // `'\\'` (one literal backslash) as escaped, so the scan runs on to the next quote in the
        // file, flips quote parity, and every reference below simply stops being indexed.
        //
        // This exact line is `modules/client_assets/client_assets.lua:96` in the engine, and it cost
        // that file all 5 of its references — including two ids that really are declared in
        // `data/styles/30-messageboxes.otui`.
        let src = "local p = tostring(path or ''):gsub('\\\\', '/')\nw:getChildById('realId')\n";
        let refs = scan_id_refs(src);
        let ids: Vec<&str> = refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["realId"],
            "the file after the gsub must still be scanned"
        );
    }

    #[test]
    fn an_unterminated_quote_costs_only_its_own_line() {
        // An LSP buffer is mid-edit most of the time, so a half-typed string is the normal case, not
        // the exotic one. A Lua short string cannot span a raw newline, so the scan stops at the
        // newline — which is what keeps a single stray quote from blanking every reference below it.
        let src = "local broken = 'oops\nw:getChildById('survivor')\n";
        let ids: Vec<String> = scan_id_refs(src).into_iter().map(|r| r.id).collect();
        assert!(
            ids.contains(&"survivor".to_owned()),
            "a stray quote must not blank the rest of the file: {ids:?}"
        );
    }

    #[test]
    fn a_level_n_long_comment_body_is_not_indexed() {
        // `--[[ … ]]` was handled, but `--[==[ … ]==]` was not: it fell through to the line-comment
        // branch, which excludes only the first line, leaving the body to be scanned as code. The
        // result is the worst kind of wrong — a clickable Location pointing inside a comment.
        let src = "--[==[\nw:getChildById('ghost')\ncontroller.ui.phantom:hide()\n]==]\nw:getChildById('realId')\n";
        let ids: Vec<String> = scan_id_refs(src).into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            ["realId"],
            "a level-N long comment must be opaque: {ids:?}"
        );
    }

    #[test]
    fn a_long_bracket_string_body_is_not_indexed() {
        // The engine has 138 long-bracket strings, some embedding literal OTUI source. Scanning
        // their bodies invents references that live inside a string — and three of them carry an odd
        // number of quotes, which also desyncs the short-string scanner for everything after.
        let src = "local code = [[ w:getChildById('inString') ]]\nw:getChildById('realId')\n";
        let ids: Vec<String> = scan_id_refs(src).into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            ["realId"],
            "a long-bracket string must be opaque: {ids:?}"
        );
    }

    #[test]
    fn a_span_is_a_byte_offset_and_survives_multibyte_text() {
        // The span becomes an LSP Location. A byte/char mix-up here puts the user's cursor on the
        // quote instead of the name — and only shows up when a non-ASCII character precedes the call.
        let src = "local t = 'ação'\nw:getChildById('mbId')\n";
        let r = scan_id_refs(src);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(
            &src[r[0].span.start..r[0].span.end],
            "mbId",
            "the span must slice to exactly the id token"
        );
    }

    #[test]
    fn a_reference_inside_a_line_comment_is_not_indexed() {
        let src = "-- widget:getChildById('closeButton')\n";
        assert!(scan_id_refs(src).is_empty());
    }

    #[test]
    fn a_reference_inside_a_block_comment_is_not_indexed() {
        let src = "--[[\nwidget:getChildById('closeButton')\n]]\nlocal x = 1\n";
        assert!(scan_id_refs(src).is_empty());
    }

    #[test]
    fn a_dot_ui_chain_inside_a_comment_is_not_indexed() {
        let src = "-- controller.ui.closeButton:setText('x')\n";
        assert!(scan_id_refs(src).is_empty());
    }

    #[test]
    fn a_reference_after_a_block_comment_is_still_indexed() {
        let src = "--[[ header comment ]]\nwidget:getChildById('closeButton')\n";
        let refs = scan_id_refs(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].id, "closeButton");
    }

    #[test]
    fn a_dot_ui_reference_not_preceded_by_an_identifier_is_ignored() {
        // A bare `.ui.foo` with nothing (identifier-wise) before the leading dot is not a member
        // access on anything and is not indexed.
        let src = "= .ui.foo\n";
        assert!(scan_id_refs(src).is_empty());
    }

    #[test]
    fn index_set_remove_and_lookup_round_trip() {
        let mut index = LuaRefIndex::new();
        assert!(index.is_empty());

        index.set_document(
            "a.lua",
            scan_id_refs("widget:getChildById('closeButton')\n"),
        );
        index.set_document(
            "b.lua",
            scan_id_refs("controller.ui.closeButton:setText('x')\n"),
        );
        assert_eq!(index.document_count(), 2);
        assert!(!index.is_empty());

        assert_eq!(
            index.document(&DocId::new("a.lua")).map(<[_]>::len),
            Some(1)
        );
        assert!(index.document(&DocId::new("missing.lua")).is_none());

        let hits = index.lookup("closeButton");
        assert_eq!(hits.len(), 2);

        // Re-indexing a document replaces its previous refs.
        index.set_document("a.lua", scan_id_refs("widget:getChildById('other')\n"));
        assert_eq!(index.lookup("closeButton").len(), 1);
        assert_eq!(index.lookup("other").len(), 1);

        let removed = index
            .remove_document(&DocId::new("b.lua"))
            .expect("was present");
        assert_eq!(removed.len(), 1);
        assert_eq!(index.document_count(), 1);
        assert!(index.remove_document(&DocId::new("b.lua")).is_none());
    }
}
