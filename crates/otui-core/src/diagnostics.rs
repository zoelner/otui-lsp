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
//!    value-validating properties of spec §2.10: `border`, `display`, `layout`, `anchors.*`), and an
//!    unknown ordinary property name ([`UNKNOWN_PROPERTY`], a *hint* — the engine silently *ignores*
//!    a tag it does not recognize, so a misspelled/unknown property never errors, it just has no
//!    effect).

use crate::catalog;
use crate::lua_widgets::LuaWidgetIndex;
use crate::schema;
use crate::style_index::StyleIndex;
use crate::syntax::SyntaxTree;
use crate::widget_resolve;
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
/// §4). Severity [`Severity::Hint`]: the engine silently *ignores* an unrecognized tag
/// (`node->tag()` matches nothing), so a misspelled/unknown property is never an error or warning —
/// only a gentle hint that the property will have no effect. Value validation of the four
/// value-validating properties (`border`, `display`, `layout`, `anchors.*`) is a separate concern;
/// this check is the unknown-KEY hint only.
pub const UNKNOWN_PROPERTY: &str = "unknown-property";
/// Diagnostic code: a malformed *value* for a value-validating property — `display`, `layout`,
/// `border`, or any **color** property (`color`, `background`, `background-color`, `icon-color`,
/// `image-color`, `ttf-stroke-color`, `border-color*` — every `node->value<Color>()` dispatch, see
/// [`catalog::COLOR_PROPERTIES`]). Severity [`Severity::Error`]: unlike an ordinary property (whose
/// bad value is silently ignored), these parse their value and the engine **throws** on malformed
/// input, so the value is a hard error. The span is the offending value token only. A `$variable`
/// reference value is exempt (it resolves at runtime — see [`leaf_value`]). (`anchors.*` has its own
/// dedicated [`INVALID_ANCHOR_EDGE`] check.)
pub const INVALID_PROPERTY_VALUE: &str = "invalid-property-value";

/// The workspace state that makes [`analyze_with_widgets`] **widget-aware**: the `.otui` style index
/// and the Lua widget index, both borrowed. Threaded down to [`check_property`] so a property the
/// C++ catalog does not know can still be accepted when the enclosing widget's Lua ancestry declares
/// it (see [`widget_resolve`]).
pub struct WidgetContext<'a> {
    /// The workspace `Name < Base` style index — resolves a widget node's `.otui` inheritance.
    pub styles: &'a StyleIndex,
    /// The workspace Lua widget index — the custom properties each widget declares.
    pub lua: &'a LuaWidgetIndex,
}

/// Computes all parse-level diagnostics for `source`, catalog-only (no widget context).
///
/// Returns findings sorted by span (`start`, then `end`). The document is parsed once; the two
/// passes share nothing beyond the source text. An unknown property is judged solely against the
/// global C++ catalog — see [`analyze_with_widgets`] for the workspace-aware variant that also
/// accepts Lua-declared custom properties.
#[must_use]
pub fn analyze(source: &str) -> Vec<Diagnostic> {
    analyze_inner(source, None)
}

/// Like [`analyze`], but **widget-aware**: a property unknown to the C++ catalog is not flagged when
/// the enclosing widget's resolved ancestry (via `ctx`) declares it as a Lua custom property. All
/// other diagnostics are identical. With empty indexes it degrades exactly to [`analyze`].
#[must_use]
pub fn analyze_with_widgets(source: &str, ctx: &WidgetContext) -> Vec<Diagnostic> {
    analyze_inner(source, Some(ctx))
}

fn analyze_inner(source: &str, ctx: Option<&WidgetContext>) -> Vec<Diagnostic> {
    let mut out = indentation_pass(source);
    // The tree is parsed once and shared by the structural and semantic passes.
    if let Some(tree) = SyntaxTree::parse(source) {
        collect_structural_errors(tree.root(), &mut out);
        collect_semantic_diagnostics(tree.root(), source, None, ctx, &mut out);
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

/// Emit a tab / odd-indentation diagnostic for `line`'s leading whitespace if malformed, returning
/// whether one was pushed. The engine checks for a tab first (`line[spaces] == '\t'`) and, only if
/// absent, for odd indentation, so at most one finding per line. Shared by the structural and comment
/// paths, since `getLineDepth` runs this on every non-blank line before `parseLine` classifies it.
fn check_indent_chars(line: &Line<'_>, out: &mut Vec<Diagnostic>) -> bool {
    let sp = leading_spaces(line.text);
    if line.text.as_bytes().get(sp) == Some(&b'\t') {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: TAB_INDENTATION,
            message: "indentation with tabs is not allowed".to_owned(),
            span: ByteSpan::new(line.start + sp, line.start + sp + 1),
        });
        true
    } else if sp % 2 != 0 {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: ODD_INDENTATION,
            message: "indentation must be a multiple of 2 spaces".to_owned(),
            span: ByteSpan::new(line.start, line.start + sp),
        });
        true
    } else {
        false
    }
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
        // Comment lines do not affect structural depth (`parseLine` skips them), but `getLineDepth`
        // runs on them FIRST, so a tab / odd indentation on a comment is still a hard engine error.
        if is_comment(trimmed) {
            check_indent_chars(line, &mut out);
            continue;
        }

        let sp = leading_spaces(line.text);
        let indent_flagged = check_indent_chars(line, &mut out);
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

/// Whether the structural line containing byte `offset` has valid indentation: spaces-only, a
/// multiple of 2, and no deeper than one level past the nearest preceding structural line. This is
/// the exact rule [`indentation_pass`] enforces (the `tab-indentation`, `odd-indentation` and
/// `invalid-indentation-depth` findings), evaluated for a single line so the `completion` module can
/// gate on it without re-deriving the depth arithmetic — the two must agree. Block-scalar **content**
/// (raw text indented under a `|` / `|-` / `|+` marker) is not structure: a cursor on such a line, or
/// on a blank/comment line, returns `false`.
pub(crate) fn line_indentation_is_valid(source: &str, offset: usize) -> bool {
    let lines = split_lines(source);
    let mut current_depth: usize = 0;
    let mut i = 0;

    // The target line is the one whose byte range (start ..= end-of-text, i.e. up to its `\n`)
    // contains `offset`.
    let contains = |line: &Line<'_>| offset >= line.start && offset <= line.start + line.text.len();

    while i < lines.len() {
        let line = &lines[i];
        i += 1;

        let trimmed = line.text.trim();
        // Blank and comment lines carry no structural depth (`parseLine` skips them). A cursor on one
        // is not a property-key position, so gate it out.
        if trimmed.is_empty() || is_comment(trimmed) {
            if contains(line) {
                return false;
            }
            continue;
        }

        let sp = leading_spaces(line.text);
        let has_tab = line.text.as_bytes().get(sp) == Some(&b'\t');
        let odd = sp % 2 != 0;
        let depth = sp / 2;
        // `parseLine`: a jump of more than one level is fatal (checked only when the line is not
        // already tab/odd-flagged, mirroring `indentation_pass`).
        let bad_jump = !has_tab && !odd && depth > current_depth + 1;

        if contains(line) {
            return !has_tab && !odd && !bad_jump;
        }

        // Advance depth exactly like the pass (set unconditionally, even for a malformed line).
        current_depth = depth;

        // Block scalars: skip their raw content lines so they neither affect structural depth nor
        // are mistaken for a structural line. A cursor landing inside the raw body returns `false`.
        if is_block_scalar_marker(line_value(trimmed)) {
            while i < lines.len() {
                let content = &lines[i];
                if content.text.trim().is_empty() {
                    if contains(content) {
                        return false;
                    }
                    i += 1;
                    continue;
                }
                if leading_spaces(content.text) > sp {
                    if contains(content) {
                        return false; // raw block content, not structure
                    }
                    i += 1;
                } else {
                    break; // next structural node — reprocess in the outer loop
                }
            }
        }
    }

    false
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
///
/// `enclosing` is the type name of the nearest ancestor widget — a `container`'s `tag` or a
/// `style_header`'s `base` — used by [`check_property`] to resolve the widget's ancestry when `ctx`
/// is present. It propagates unchanged through non-widget nodes (e.g. a `$state` block's properties
/// belong to the same widget) and is overridden when entering a new widget.
fn collect_semantic_diagnostics<'a>(
    node: Node<'_>,
    source: &'a str,
    enclosing: Option<&'a str>,
    ctx: Option<&WidgetContext>,
    out: &mut Vec<Diagnostic>,
) {
    match node.kind() {
        "state_selector" => check_state_selector(node, source, out),
        "anchor_property" => check_anchor_property(node, source, out),
        "property" => {
            check_property(node, source, enclosing, ctx, out);
            // Inside a `layout:` block the keys are consumed by the layout object, never by the
            // widget style parser, so none of the value-validating families apply there — a
            // `display:` nested under `layout:` is not a display value the engine would parse (and
            // throw on), it is just a key the layout ignores. Validating it would be a false-positive
            // error. The `layout` node itself is not inside a block, so its own value (including the
            // block's `type:`) is still validated.
            if enclosing_block_key(node, source) != Some("layout") {
                check_property_value(node, source, out);
            }
        }
        _ => {}
    }
    // The enclosing widget type for this node's children: a widget node sets it from its own type,
    // any other node passes the current one through. A `style_header`'s children are properties of
    // an instance that is-a its `base`, so `base` (not the declared name, a user style with no Lua
    // props) is the type to resolve; a `container`'s children belong to its `tag`.
    let child_enclosing = match node.kind() {
        "style_header" => node.child_by_field_name("base").map(|b| slice(source, b)),
        "container" => node.child_by_field_name("tag").map(|t| slice(source, t)),
        _ => enclosing,
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_semantic_diagnostics(child, source, child_enclosing, ctx, out);
    }
}

/// The key of the `property` **block** this node sits directly inside, or `None` when the node is not
/// nested in one (a top-level widget property, a list item, a `$state` block's child, …).
///
/// The grammar's `_block` is a hidden rule, so a block's statements are inlined as direct children of
/// the owning `property` node: a `type:` inside `layout:` has the `layout` property node as its
/// parent. A parent of any other kind (`container`, `state_selector`, `document`, …) is not a
/// property block and yields `None`.
fn enclosing_block_key<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let parent = node.parent()?;
    if parent.kind() != "property" {
        return None;
    }
    parent
        .child_by_field_name("key")
        .map(|key| slice(source, key))
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

    // Target value: for a **real** edge (not the `fill`/`centerIn` shorthands) with a value other
    // than `none`, the engine requires exactly `<id>.<edge>` — `split(value, ".")` must have two
    // parts, else it throws `"invalid anchor description"`. So a dot-less target (`anchors.top:
    // parent`) or a multi-dot one (`parent.top.bottom`) is a hard error; `none`, and any target on a
    // shorthand edge (which takes a plain id), are exempt.
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    let Some(target) = value.child_by_field_name("target") else {
        return;
    };
    let text = slice(source, target);

    let edge_is_shorthand = node
        .child_by_field_name("edge")
        .is_some_and(|e| schema::is_shorthand_anchor(slice(source, e)));
    if text == "none" || edge_is_shorthand {
        return;
    }

    // Exactly one `.` (a two-part `id.edge`). Zero or two-plus is an "invalid anchor description".
    if text.matches('.').count() != 1 {
        out.push(Diagnostic {
            severity: Severity::Error,
            code: INVALID_ANCHOR_EDGE,
            message: format!("`{text}` is not a valid anchor target: expected `<id>.<edge>`"),
            span: SyntaxTree::span_of(target),
        });
        return;
    }

    // The suffix after the (single) dot must be an anchor edge. A trailing-dot / empty suffix is left
    // unflagged (prefer a false negative on a shape the grammar does not cleanly delimit).
    let dot = text.find('.').expect("exactly one dot");
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
/// (`container`/`style_header`). Property VALUES are intentionally not validated here (the
/// `border`/`display`/`layout`/`anchors.*` value validation is a separate node); this is the
/// unknown-KEY hint only.
///
/// Membership is [`schema::is_known_property`], an exact case-sensitive compare (the engine
/// dispatches on `node->tag() == "..."`), so a mis-cased `Width` is unknown → hint. That is faithful.
///
/// A `property` that carries a nested block (`key:` with indented children) is *not* flagged: the
/// grammar spells a colon-keyed group as a `property` with a `_block`, but such a node acts as a
/// container/subtree (a child widget group, or a `key:`/`- item` list parent), not a leaf style
/// property. Per the "prefer false negatives over false positives" rule, we only flag leaf
/// properties (a bare `key:` or `key: value` with no nested statements).
///
/// When a [`WidgetContext`] is present, a key the catalog does not know is additionally checked
/// against the enclosing widget's Lua ancestry: OTClient widgets add style properties in Lua
/// (e.g. a `UITable` reads `column-style`), so such a property is valid — not unknown — on a node
/// whose type descends from the declaring widget. Only when it is in neither the catalog nor the
/// resolved ancestry is the hint emitted.
fn check_property(
    node: Node<'_>,
    source: &str,
    enclosing: Option<&str>,
    ctx: Option<&WidgetContext>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(key) = node.child_by_field_name("key") else {
        return;
    };
    // Detect a nested block: any *named* child that is neither the `key` field nor the `value` field
    // is a statement inside a `_block`, so the node is a container form — skip it.
    let value = node.child_by_field_name("value");
    let mut cursor = node.walk();
    let has_block = node
        .named_children(&mut cursor)
        // `map_or(true, …)` rather than `Option::is_none_or` — the latter is only stable since Rust
        // 1.82, but the workspace MSRV is 1.75.
        .any(|child| child.id() != key.id() && value.map_or(true, |v| child.id() != v.id()));
    if has_block {
        return;
    }
    let name = slice(source, key);
    // A key nested directly under a `layout:` block is read by the *layout object*
    // (`UIBoxLayout`/`UIGridLayout`/…`::applyStyle`), not the widget style parser, so it lives in its
    // own catalog. The grammar's `_block` is a hidden rule, so a block's statements are direct
    // children of the `property` node — the enclosing block is simply this node's parent.
    //
    // Checked *before* the widget catalog and deliberately exclusive: these keys are valid only here
    // (a bare `cell-size:` on a widget really is unknown), and conversely a widget property is not
    // valid inside a `layout:` block, so the block context replaces the widget check rather than
    // adding to it.
    if let Some(block) = enclosing_block_key(node, source) {
        if block == "layout" {
            if schema::is_layout_block_property(name) {
                return;
            }
            out.push(Diagnostic {
                severity: Severity::Hint,
                code: UNKNOWN_PROPERTY,
                message: format!(
                    "unknown layout property `{name}`: ignored by the engine, has no effect"
                ),
                span: SyntaxTree::span_of(key),
            });
            return;
        }
    }
    if schema::is_known_property(name) {
        return;
    }
    // Widget-aware acceptance: a catalog-unknown key that the enclosing widget (or a Lua ancestor)
    // declares is a valid Lua-added property, not an unknown one.
    if let (Some(ctx), Some(widget)) = (ctx, enclosing) {
        if widget_resolve::resolve_ancestry(widget, ctx.styles, ctx.lua)
            .declares_custom_property(ctx.lua, name)
        {
            return;
        }
    }
    out.push(Diagnostic {
        severity: Severity::Hint,
        code: UNKNOWN_PROPERTY,
        message: format!("unknown property `{name}`: ignored by the engine, has no effect"),
        span: SyntaxTree::span_of(key),
    });
}

/// Validate the *value* of one of the value-validating properties `display`, `layout`, `border`, and
/// `border-color*` (spec §2.10, §4). Unlike an unknown property *key* (a hint) or an ordinary
/// property's value (never validated), these families parse their value and the engine **throws** on
/// malformed input, so a bad value is an [`INVALID_PROPERTY_VALUE`] error spanning the offending value
/// token. (`anchors.*`, the fourth value-validating property, is checked separately in
/// [`check_anchor_property`].)
///
/// Property tags are matched **exactly** — the engine dispatches on `node->tag() == "..."` — so only
/// the canonical lowercase/kebab spelling triggers validation; a mis-cased `Display:` is an unknown
/// property (an unrelated hint) and is not value-validated here.
///
/// Faithfulness notes, each mirroring an observed engine behavior:
/// * `display` — the engine lowercases the value, then matches it against a fixed set; an unknown
///   value throws. Validated case-insensitively to match ([`schema::is_display_value`]).
/// * `layout` — the type is taken from the leaf value (`layout: <type>`) or a nested `type:` child
///   (`layout:` block); a **non-empty** type outside the fixed set throws. Matched case-sensitively,
///   as the engine does. An absent/empty type does *not* throw (the engine leaves the layout unset).
/// * `border` — the shorthand must resolve to both a width and a color (or be the single
///   `none`/`hidden` keyword); otherwise the engine throws ([`schema::is_valid_border`]).
/// * `border-color*` — the value must parse as a color; the engine throws otherwise
///   ([`schema::is_border_color_value`]). The numeric `border-width*` sub-properties are deliberately
///   **not** validated: the engine reads them with a lenient digit-scanning converter that never
///   throws (a non-numeric value is silently coerced to 0), so they are not an error.
///
/// Only a non-empty leaf `key: value` is validated for `display`/`border`/`border-color*` (an empty
/// value or a block form is skipped — a false negative is preferred over a false positive on a shape
/// the engine would treat differently); `layout` additionally handles its block form.
fn check_property_value(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    let Some(key) = node.child_by_field_name("key") else {
        return;
    };
    match slice(source, key) {
        "display" => {
            if let Some((text, span)) = leaf_value(node, source) {
                if !schema::is_display_value(text) {
                    push_invalid_value(
                        out,
                        format!("`{text}` is not a valid `display` value"),
                        span,
                    );
                }
            }
        }
        "layout" => check_layout_value(node, source, out),
        "border" => {
            if let Some((text, span)) = leaf_value(node, source) {
                if !schema::is_valid_border(text) {
                    push_invalid_value(
                        out,
                        format!(
                            "`{text}` is not a valid `border` value: expected a width and a color"
                        ),
                        span,
                    );
                }
            }
        }
        // Every color-typed property (`color`, `background`, `background-color`, `icon-color`,
        // `image-color`, `ttf-stroke-color`, and the `border-color*` family) is applied via
        // `node->value<Color>()`, which THROWS on a value it cannot parse — so a bad color is a hard
        // engine error, not just for `border-color`. Validate the whole set from the catalog.
        //
        // A `[a, b]` list is included here on purpose: OTML parses `key: [a, b]` by splitting on `,`
        // and writing each item as a *child* node (otmlparser `writeIn`), which leaves the color
        // node's own value empty — so `value<Color>()` casts the empty string and throws. A color
        // property takes a single color, never a list, so `leaf_value` (which yields the whole
        // `[...]` token) flagging it is faithful, matching how `border`/`display`/`layout` treat a
        // list too.
        k if catalog::COLOR_PROPERTIES.contains(&k) => {
            if let Some((text, span)) = leaf_value(node, source) {
                if !schema::is_border_color_value(text) {
                    let message = if text.starts_with('[') {
                        format!("`{text}`: a color property takes a single color, not a list")
                    } else {
                        format!("`{text}` is not a valid color")
                    };
                    push_invalid_value(out, message, span);
                }
            }
        }
        _ => {}
    }
}

/// Validate a `layout` property's type against [`schema::is_layout_type`], handling both the leaf
/// form (`layout: <type>`) and the block form (`layout:` with a nested `type: <type>` child) — the
/// engine resolves the type from whichever is present. A non-empty invalid type is an error; an
/// absent/empty type is not (the engine leaves the layout unset without throwing).
fn check_layout_value(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    // Leaf form: `layout: <type>`.
    if let Some((text, span)) = leaf_value(node, source) {
        if !schema::is_layout_type(text) {
            push_invalid_value(out, format!("`{text}` is not a valid `layout` type"), span);
        }
        return;
    }
    // Block form: `layout:` with a nested `type: <type>` child property.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "property" {
            continue;
        }
        let Some(child_key) = child.child_by_field_name("key") else {
            continue;
        };
        if slice(source, child_key) != "type" {
            continue;
        }
        if let Some((text, span)) = leaf_value(child, source) {
            if !schema::is_layout_type(text) {
                push_invalid_value(out, format!("`{text}` is not a valid `layout` type"), span);
            }
        }
        return;
    }
}

/// The trimmed text and byte span of a property's leaf `value` field, or `None` when the property has
/// no value field (a block form or a bare `key:`) or its value is empty/whitespace. The span covers
/// exactly the trimmed value token, so a diagnostic points at the value rather than its surrounding
/// whitespace.
fn leaf_value<'a>(node: Node<'_>, source: &'a str) -> Option<(&'a str, ByteSpan)> {
    let value = node.child_by_field_name("value")?;
    let raw = slice(source, value);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // A `$variable` reference resolves to its value at runtime, so we cannot statically validate it —
    // and must not flag it. (`$var` is the OTML variable form; a value never legitimately starts with
    // `$` otherwise.) This keeps every value-validating property from false-flagging `key: $var`.
    if trimmed.starts_with('$') {
        return None;
    }
    let lead = raw.len() - raw.trim_start().len();
    let start = value.start_byte() + lead;
    Some((trimmed, ByteSpan::new(start, start + trimmed.len())))
}

/// Push an [`INVALID_PROPERTY_VALUE`] error (`Severity::Error`) spanning the offending value token.
fn push_invalid_value(out: &mut Vec<Diagnostic>, message: String, span: ByteSpan) {
    out.push(Diagnostic {
        severity: Severity::Error,
        code: INVALID_PROPERTY_VALUE,
        message,
        span,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code).collect()
    }

    #[test]
    fn line_indentation_is_valid_mirrors_the_pass() {
        // Byte offset of `needle` in `src`.
        let at = |src: &str, needle: &str| src.find(needle).expect("needle present");

        // Valid +1 level under a widget.
        let src = "Panel\n  wid\n";
        assert!(line_indentation_is_valid(src, at(src, "wid") + 1));

        // Odd (3-space) indentation → invalid.
        let src = "Panel\n   wid\n";
        assert!(!line_indentation_is_valid(src, at(src, "wid") + 1));

        // Depth jump of two levels (0 → 2) → invalid.
        let src = "Panel\n    wid\n";
        assert!(!line_indentation_is_valid(src, at(src, "wid") + 1));

        // Tab indentation → invalid.
        let src = "Panel\n\twid\n";
        assert!(!line_indentation_is_valid(src, at(src, "wid") + 1));

        // A cursor inside a `|` block-scalar body is raw text, not structure → invalid.
        let src = "Panel\n  @onClick: |\n    some\n";
        assert!(!line_indentation_is_valid(src, at(src, "some") + 1));

        // Valid +1 under a nested widget (depth 2 under depth 1).
        let src = "Panel\n  Child\n    wid\n";
        assert!(line_indentation_is_valid(src, at(src, "wid") + 1));
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

    // --- semantic pass: `layout:` block keys ---------------------------------------------------

    #[test]
    fn layout_block_keys_are_not_unknown_properties() {
        // Regression: every key inside a `layout:` block is read by the layout object
        // (`UIGridLayout::applyStyle`), not the widget style parser, so none is an unknown property.
        // Before this was handled, each of these raised a false `unknown property` hint.
        let src = "\
Panel
  layout:
    type: grid
    cell-size: 32 32
    cell-spacing: 2
    num-columns: 4
    fit-children: true
    flow: true
";
        assert!(
            analyze(src).is_empty(),
            "a grid `layout:` block must be silent, got {:?}",
            analyze(src)
        );
    }

    #[test]
    fn box_layout_block_keys_are_not_unknown_properties() {
        // The box family: `spacing`/`fit-children` from `UIBoxLayout`, `align-bottom` from
        // `UIVerticalLayout`, `align-right` from `UIHorizontalLayout`.
        let src = "\
Panel
  layout:
    type: verticalBox
    spacing: 4
    fit-children: true
    align-bottom: true
";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn real_otclient_layout_fixture_is_silent() {
        // Copied from the engine's own `data/styles/30-inputboxes.otui` — content the engine loads
        // without complaint, so the LSP must not flag it.
        let src = "\
Panel
  layout:
    type: horizontalBox
    spacing: 8
";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn layout_leaf_form_still_works() {
        // The leaf form (`layout: <type>`) has no block, so it is value-validated as before.
        assert!(analyze("Panel\n  layout: verticalBox\n").is_empty());
        let diags = analyze("Panel\n  layout: bogusLayout\n");
        assert_eq!(
            only(&diags, INVALID_PROPERTY_VALUE).severity,
            Severity::Error
        );
    }

    #[test]
    fn unknown_key_inside_layout_block_is_still_hinted() {
        // The block context accepts the layout catalog, it does not blanket-accept: a key no layout
        // class reads is still ignored by the engine, so it stays a hint.
        let src = "Panel\n  layout:\n    type: grid\n    cell-siz: 32 32\n";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "cell-siz");
    }

    #[test]
    fn layout_key_at_widget_level_is_still_unknown() {
        // The layout catalog is exclusive to the block: a bare `cell-size:` on a widget is read by
        // nobody, so it remains an unknown property.
        let src = "Panel\n  cell-size: 32 32\n";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "cell-size");
    }

    #[test]
    fn value_families_are_not_validated_inside_a_layout_block() {
        // A `display:` nested under `layout:` is never parsed as a display value by the engine (the
        // layout object reads the block and ignores the key), so it must not raise a value error —
        // only the unknown-layout-key hint.
        let src = "Panel\n  layout:\n    type: grid\n    display: bogus\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "a key inside a layout block must not be value-validated, got {diags:?}"
        );
        assert_eq!(only(&diags, UNKNOWN_PROPERTY).severity, Severity::Hint);
    }

    // --- semantic pass: invalid property value (error) ----------------------------------------

    #[test]
    fn valid_display_value_is_silent() {
        let src = "Panel\n  display: flex\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "valid display must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn invalid_display_value_is_an_error_on_the_value() {
        let src = "Panel\n  display: blocky\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "blocky");
    }

    #[test]
    fn miscased_display_value_is_accepted() {
        // The engine lowercases the value before matching, so `Flex` is valid.
        let src = "Panel\n  display: Flex\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "case-insensitive display value must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn valid_layout_leaf_value_is_silent() {
        let src = "Panel\n  layout: verticalBox\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "valid layout must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn invalid_layout_leaf_value_is_an_error_on_the_value() {
        let src = "Panel\n  layout: bogusBox\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "bogusBox");
    }

    #[test]
    fn miscased_layout_type_is_an_error() {
        // The engine compares the layout type verbatim (no lowercasing), so `verticalbox` is invalid.
        let src = "Panel\n  layout: verticalbox\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(&src[d.span.start..d.span.end], "verticalbox");
    }

    #[test]
    fn valid_layout_block_type_is_silent() {
        // Block form: the engine reads the type from the nested `type:` child.
        let src = "\
Panel
  layout:
    type: grid
    cell-size: 32 32
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "valid block-form layout must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn invalid_layout_block_type_is_an_error_on_the_type_value() {
        let src = "\
Panel
  layout:
    type: bogusBox
";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "bogusBox");
    }

    #[test]
    fn layout_block_without_type_is_not_an_error() {
        // No leaf value and no `type:` child -> the engine leaves the layout unset without throwing.
        let src = "\
Panel
  layout:
    cell-size: 32 32
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "typeless layout block must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn valid_border_shorthand_is_silent() {
        let src = "Panel\n  border: 2 solid red\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "valid border must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn border_none_keyword_is_silent() {
        let src = "Panel\n  border: none\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "`border: none` must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn border_with_only_a_color_is_an_error() {
        // A color without a width: the engine throws (`border must include width and color`).
        let src = "Panel\n  border: red\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "red");
    }

    #[test]
    fn border_with_only_a_width_is_an_error() {
        let src = "Panel\n  border: 3\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "3");
    }

    #[test]
    fn valid_border_color_is_silent() {
        let src = "Panel\n  border-color: #ff0000\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "valid border-color must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn invalid_border_color_is_an_error() {
        let src = "Panel\n  border-color-top: notacolor\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_PROPERTY_VALUE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "notacolor");
    }

    #[test]
    fn nonnumeric_border_width_is_not_validated() {
        // The engine reads `border-width*` with a lenient digit-scanning converter that never throws,
        // so a non-numeric value is silently coerced, not an error.
        let src = "Panel\n  border-width: fat\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "border-width is not value-validated: {diags:?}"
        );
    }

    #[test]
    fn ordinary_property_with_a_weird_value_is_not_a_value_error() {
        // `width` is a known property but NOT one of the four value-validating ones, so a nonsense
        // value is never an invalid-value error (the engine silently coerces/ignores it).
        let src = "Panel\n  width: not-a-number\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "non-validating property value must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn value_validation_only_fires_for_the_named_properties() {
        // A mis-cased `Display` is an unknown property (a hint), not value-validated even though its
        // value would be invalid for the real `display` property.
        let src = "Panel\n  Display: blocky\n";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
            "mis-cased property must not be value-validated: {diags:?}"
        );
    }

    // --- F1: every color property validates its value (engine throws on a bad color) ----------

    #[test]
    fn bad_color_value_is_an_error_on_every_color_property() {
        for key in ["color", "background", "background-color", "icon-color"] {
            let src = format!("Panel\n  {key}: notacolor\n");
            let diags = analyze(&src);
            let d = only(&diags, INVALID_PROPERTY_VALUE);
            assert_eq!(d.severity, Severity::Error);
            assert_eq!(&src[d.span.start..d.span.end], "notacolor", "for `{key}`");
        }
    }

    #[test]
    fn valid_color_on_a_plain_color_property_is_silent() {
        for src in [
            "Panel\n  color: #ff0000\n",
            "Panel\n  background-color: red\n",
        ] {
            let diags = analyze(src);
            assert!(
                diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
                "valid color must not be flagged: {diags:?}"
            );
        }
    }

    #[test]
    fn a_color_list_value_is_flagged_because_the_engine_cannot_cast_it() {
        // OTML parses `color: [red, #00ff00]` into child nodes (otmlparser `writeIn`), leaving the
        // color node's own value empty, so `node->value<Color>()` casts "" and throws. A color
        // property takes a single color, never a list — so the whole `[...]` token is an error.
        for src in [
            "Panel\n  color: [red, #00ff00]\n",
            "Panel\n  background-color: [red, blue]\n",
        ] {
            let diags = analyze(src);
            let d = only(&diags, INVALID_PROPERTY_VALUE);
            assert_eq!(d.severity, Severity::Error);
            let flagged = &src[d.span.start..d.span.end];
            assert!(
                flagged.starts_with('[') && flagged.ends_with(']'),
                "flags the list: {flagged:?}"
            );
            assert!(
                d.message.contains("single color, not a list"),
                "message: {}",
                d.message
            );
        }
    }

    #[test]
    fn a_variable_reference_value_is_never_flagged() {
        // `$var` resolves at runtime, so no value-validating property (color, display, layout,
        // border) may flag it (regression guard for the false-positive the color widening exposed).
        for src in [
            "Panel\n  color: $textColor\n",
            "Panel\n  background-color: $bg\n",
            "Panel\n  display: $mode\n",
            "Panel\n  layout: $l\n",
            "Panel\n  border-color: $c\n",
        ] {
            let diags = analyze(src);
            assert!(
                diags.iter().all(|d| d.code != INVALID_PROPERTY_VALUE),
                "a $var value must not be flagged: {src:?} -> {diags:?}"
            );
        }
    }

    // --- F2: an anchor target must be `<id>.<edge>` (one dot) on a real edge -------------------

    #[test]
    fn dotless_anchor_target_on_a_real_edge_is_an_error() {
        let src = "Widget\n  anchors.left: parent\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_ANCHOR_EDGE);
        assert_eq!(d.severity, Severity::Error);
        assert!(&src[d.span.start..d.span.end] == "parent");
    }

    #[test]
    fn multidot_anchor_target_is_an_error() {
        let src = "Widget\n  anchors.top: parent.top.bottom\n";
        let diags = analyze(src);
        let d = only(&diags, INVALID_ANCHOR_EDGE);
        assert_eq!(&src[d.span.start..d.span.end], "parent.top.bottom");
    }

    #[test]
    fn valid_and_exempt_anchor_targets_are_silent() {
        // `id.edge` targets, `none`, and the fill/centerIn shorthands (plain target) are all fine.
        for src in [
            "Widget\n  anchors.top: parent.top\n",
            "Widget\n  anchors.left: sibling.right\n",
            "Widget\n  anchors.bottom: none\n",
            "Widget\n  anchors.fill: parent\n",
            "Widget\n  anchors.centerIn: parent\n",
        ] {
            let diags = analyze(src);
            assert!(
                diags.iter().all(|d| d.code != INVALID_ANCHOR_EDGE),
                "must be silent: {src:?} -> {diags:?}"
            );
        }
    }

    // --- F3: tab / odd indentation on a comment line is a hard error ---------------------------

    #[test]
    fn tab_indented_comment_is_flagged() {
        let src = "Panel\n\t// note\n";
        assert_eq!(codes(&analyze(src)), vec![TAB_INDENTATION]);
    }

    #[test]
    fn odd_indented_comment_is_flagged() {
        let src = "Panel\n   # note\n"; // 3 spaces
        assert_eq!(codes(&analyze(src)), vec![ODD_INDENTATION]);
    }

    #[test]
    fn a_validly_indented_comment_is_silent() {
        let src = "Panel\n  // fine\n  id: main\n";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    // --- widget-aware unknown property (Lua-added style properties) ----------------------------

    use crate::lua_widgets::{scan_widgets, LuaWidgetIndex};
    use crate::style_index::{extract_style_defs, StyleIndex};

    /// A [`StyleIndex`] built from `(doc, otui_source)` pairs.
    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(*doc, extract_style_defs(&tree));
        }
        index
    }

    /// A [`LuaWidgetIndex`] built from `(doc, lua_source)` pairs.
    fn lua(docs: &[(&str, &str)]) -> LuaWidgetIndex {
        let mut index = LuaWidgetIndex::new();
        for (doc, src) in docs {
            index.set_document(*doc, scan_widgets(src));
        }
        index
    }

    /// The `uitable.lua` shape used across the widget-aware tests: `UITable` extends `UIWidget` and
    /// declares `column-style` in its `onStyleApply`.
    const UITABLE_LUA: &str = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    end
  end
end
";

    #[test]
    fn lua_property_on_a_matching_style_header_is_accepted() {
        // `column-style` is not a C++ catalog property, but on a `< UITable` header it is a valid
        // Lua-added property, so no hint.
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Table < UITable\n  column-style: SomeColumn\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_PROPERTY),
            "column-style on a UITable must be accepted: {diags:?}"
        );
    }

    #[test]
    fn lua_property_on_a_widget_instance_tag_is_accepted_cross_file() {
        // The property sits on a nested `Table` container whose type resolves cross-file
        // (`Table < UITable`, defined in the workspace index) to the native UITable.
        let styles = styles(&[("lib.otui", "Table < UITable\n")]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "\
Window < UIWindow
  Table
    column-style: SomeColumn
";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_PROPERTY),
            "column-style on a Table instance must be accepted: {diags:?}"
        );
    }

    #[test]
    fn lua_property_on_an_unrelated_widget_still_hints() {
        // `column-style` on a Button (which does not descend from UITable) is still unknown.
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Button < UIButton\n  column-style: SomeColumn\n";
        let diags = analyze_with_widgets(src, &ctx);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "column-style");
    }

    #[test]
    fn genuinely_misspelled_property_still_hints_with_context() {
        // A real typo (`widht`) is unknown to both the catalog and the widget's Lua ancestry.
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Table < UITable\n  widht: 10\n";
        let diags = analyze_with_widgets(src, &ctx);
        let d = only(&diags, UNKNOWN_PROPERTY);
        assert_eq!(&src[d.span.start..d.span.end], "widht");
    }

    #[test]
    fn empty_context_degrades_to_catalog_only() {
        // With no styles and no Lua widgets, a Lua-added property is (correctly) still just a hint —
        // identical to catalog-only `analyze`.
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Table < UITable\n  column-style: SomeColumn\n";
        let with = analyze_with_widgets(src, &ctx);
        let without = analyze(src);
        assert_eq!(
            only(&with, UNKNOWN_PROPERTY).span,
            only(&without, UNKNOWN_PROPERTY).span
        );
    }
}
