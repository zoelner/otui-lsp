//! Diagnostics (spec §4), a faithful mirror of the OTClient OTML parser and style resolver.
//!
//! Severity is **not** uniform here, and that is the point: it tracks what the *engine* does with
//! each mistake. A condition the engine treats as fatal (`OTMLException`), or that leaves the
//! tree-sitter grammar unable to form a valid node, is a [`Severity::Error`]. A mistake the engine
//! silently tolerates is a *hint* — never an error, however wrong it looks (spec §2.10). Style
//! resolution sits between the two: a dangling base still registers its style, so it is a
//! [`Severity::Warning`]; an unresolvable *root* instance means the file does not load, so it is an
//! error.
//!
//! Four passes contribute:
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
//!    effect). The same pass also flags a purely **structural** ordering mistake that needs no
//!    schema lookup at all: a plain property declared after a child widget in the same body
//!    ([`PROPERTY_AFTER_CHILD`], a *hint* — `UIManager::createWidgetFromOTML` applies every property
//!    of a node before creating any of its children, so the textual order is semantically
//!    irrelevant to the engine; a `$state` selector block is exempt, since it idiomatically reads
//!    last).
//! 4. A **style-resolution** pass over the top-level nodes only ([`UNKNOWN_BASE`], a *warning*, and
//!    [`UNKNOWN_ROOT_STYLE`], an *error*). This one is **workspace-aware**: it needs the
//!    [`WidgetContext`] and therefore fires only under [`analyze_with_widgets`] — plain [`analyze`]
//!    has no style index and must never invent a resolution failure it cannot actually check.

use crate::catalog;
use crate::indent::{
    is_block_scalar_marker, is_comment, leading_spaces, line_value, split_lines, Line,
};
use crate::lua_widgets::LuaWidgetIndex;
use crate::schema;
use crate::style_index::{is_native_base, StyleIndex};
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

/// Diagnostic code: an ordinary property declared textually *after* a child widget within the same
/// body. Severity [`Severity::Hint`]: in `UIManager::createWidgetFromOTML` (`uimanager.cpp:706`) the
/// widget applies **every** property of its own style node (`widget->setStyleFromNode`) before it
/// creates any of its children — the property's position relative to a child widget changes nothing
/// at runtime, so this is a readability rule, not something the engine cares about. A `$state`
/// selector block (`$on:`, `$hover:`, …) is exempt: it is a conditional override that idiomatically
/// reads last, so it never triggers this hint no matter where it sits.
pub const PROPERTY_AFTER_CHILD: &str = "property-after-child";

/// An `anchors.*` on a widget whose parent explicitly uses a non-anchor layout
/// (`layout: horizontalBox` / `verticalBox` / `grid`).
///
/// An **error**: the engine throws `"cannot create anchor, the parent widget doesn't use anchor
/// layout!"` while applying the style, so the file fails to load. Every widget defaults to
/// `UIAnchorLayout`, so this only fires when the parent *switched* layout and a child under it still
/// anchors.
pub const ANCHOR_PARENT_NO_ANCHOR_LAYOUT: &str = "anchor-parent-no-anchor-layout";
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

/// Diagnostic code: a top-level `Name < Base` style declaration whose `Base` resolves to no
/// workspace style and is not a native `UI*` built-in (spec §2.2, §4.2). Severity
/// [`Severity::Warning`], **not** error: `UIManager::importStyleFromOTML` still registers `Name`
/// into the global style namespace even when `Base` cannot be found — nothing about parsing or
/// loading the *declaring* file fails, so a dangling base only ever bites whoever later
/// instantiates `Name` (a separate, already-covered resolution). Widget-aware, so it only fires
/// under [`analyze_with_widgets`] — see the [`WidgetContext`] field docs.
pub const UNKNOWN_BASE: &str = "unknown-base";
/// Diagnostic code: the file's root/main-widget instance — its one top-level `container` (a bare
/// tag, no `< Base`, spec §2.2 `UIManager::findMainWidgetNode`) — whose tag resolves to no
/// workspace style and is not a native `UI*` built-in. Severity [`Severity::Error`]: unlike
/// [`UNKNOWN_BASE`], this node is the one the file actually instantiates
/// (`UIManager::loadUIFromString` → `UIManager::getStyle` on the root node's tag), so an unknown
/// style here means the file itself fails to load. A `.otui` fragment with no such root instance —
/// only `Name < Base` declarations — is a valid "style-only" file and never triggers this check
/// (spec §4.2). Widget-aware, so it only fires under [`analyze_with_widgets`].
pub const UNKNOWN_ROOT_STYLE: &str = "unknown-root-style";

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
        check_top_level_style_resolution(tree.root(), source, ctx, &mut out);
    }
    out.sort_by_key(|d| (d.span.start, d.span.end));
    out
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
    } else if !sp.is_multiple_of(2) {
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
        let odd = !sp.is_multiple_of(2);
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
    // Purely structural, needs no schema/catalog lookup, so it runs unconditionally over every
    // node's own immediate children before the kind-specific checks below. See
    // `check_property_after_child` for the ordering rule.
    check_property_after_child(node, out);
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
        // Prefer the **declared name** over the base. Resolving from the name walks the same
        // `< Base` chain, but it also picks up a `__class:` re-root declared in this very header's
        // body — which resolving from the base cannot see, since the re-root lives on the name's own
        // `StyleDef`. Without this, a `minimum:` written inside `SpinBox < TextEdit` /
        // `__class: UISpinBox` resolves against `TextEdit` and is wrongly hinted, even though the
        // same property on a `SpinBox` *instance* elsewhere resolves fine.
        //
        // Fall back to the base when the name is not in the style index (an un-indexed buffer):
        // that is exactly what resolving from the base always did, so the degenerate case cannot
        // regress into *more* hints.
        "style_header" => {
            let name = node.child_by_field_name("name").map(|n| slice(source, n));
            let base = node.child_by_field_name("base").map(|b| slice(source, b));
            match (ctx, name) {
                (Some(ctx), Some(name)) if !ctx.styles.lookup(name).is_empty() => Some(name),
                _ => base,
            }
        }
        "container" => node.child_by_field_name("tag").map(|t| slice(source, t)),
        _ => enclosing,
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_semantic_diagnostics(child, source, child_enclosing, ctx, out);
    }
}

/// Flag every plain `property` sibling that follows a `container` (child widget) among `node`'s own
/// **direct** named children — the ordering mistake of spec's readability rule, see
/// [`PROPERTY_AFTER_CHILD`].
///
/// The grammar's `_block` is a hidden rule, so a widget body's statements (properties, child
/// widgets, `$state` blocks, …) are direct children of the owning `container` / `style_header` node
/// — and likewise for any other node that owns a block (a `state_selector`, or a `property` with a
/// nested block such as `layout:`). Calling this once per node, over that node's own children only,
/// therefore covers every block in the tree exactly once: "reset per block" falls out naturally from
/// each call starting its own fresh `seen_child`, independent of its caller's.
///
/// Only the `property` kind is flagged, and the rule only runs inside a widget body (a `container` or
/// a `style_header`) — see the allowlist in the body for the three places a bare tag is *not* a child
/// widget.
///
/// Two deliberate exemptions, both of which would otherwise be noisy false positives:
///
/// * A `state_selector` (`$state` block) is a distinct node kind and is never flagged even when it
///   textually follows a child — a `$state` block is a conditional override that idiomatically reads
///   last (spec §2.8), and it is 14 of the 18 occurrences in the engine corpus.
/// * An `id_property`, `anchor_property`, `event_property`, `alias_property` or `expr_property`
///   (`id:`, `anchors.*`, `@event`, `&alias`, `!expr`) after a child is **also** not flagged: each is
///   its own grammar kind, not `property`. This is a deliberate under-approximation — it occurs zero
///   times in the engine corpus, so it costs no real coverage, and widening the rule is only worth
///   doing if a real case ever turns up.
///
/// Scanning **stops** at the first `ERROR`/`MISSING` sibling. A malformed line (already reported by
/// [`collect_structural_errors`] as its own [`SYNTAX_ERROR`]) can throw the parser into error
/// recovery, which sometimes synthesizes a bogus `container` node out of the wreckage of the
/// following text — that recovery artifact is not a real child widget, and everything after it in
/// this block is unreliable to classify, so we do not pile a readability hint on top of a syntax
/// error.
fn check_property_after_child(node: Node<'_>, out: &mut Vec<Diagnostic>) {
    // Only a **widget's own body** can contain "a property after a child widget". A widget is spelled
    // two ways — a bare tag (`Button`, a `container`) or a tag with an inline base
    // (`Label < UILabel`, a `style_header`) — and those two kinds are the only blocks whose bare-tag
    // children the engine actually instantiates. Everywhere else, a bare tag means something else
    // entirely and this rule does not apply:
    //
    // * the **document root**, which has no widget body: a `Name < Base` there is a style
    //   *declaration* (spec §2.2), and a stray root-level property is a different error altogether;
    // * a **`$state` block** (`$on:`): the line carries a `:`, so `OTMLParser` marks the node
    //   *unique* (`otmlparser.cpp:435`), and `createWidgetFromOTML`'s `if (!childNode->isUnique())`
    //   child loop never descends into it — a tag in there instantiates nothing;
    // * a **data block** (`options:` with bare-tag entries), whose children are values read from Lua
    //   via `pairs(styleNode.options)`, not widgets.
    //
    // Emitting the hint in any of those would state something false ("the engine applies all
    // properties before creating any children") about nodes that are not children at all. So this is
    // an allowlist, never a denylist.
    if !matches!(node.kind(), "container" | "style_header") {
        return;
    }
    let mut cursor = node.walk();
    let mut seen_child = false;
    for child in node.named_children(&mut cursor) {
        if child.is_error() || child.is_missing() {
            break;
        }
        match child.kind() {
            "container" | "style_header" => seen_child = true,
            "property" if seen_child => {
                if let Some(key) = child.child_by_field_name("key") {
                    out.push(Diagnostic {
                        severity: Severity::Hint,
                        code: PROPERTY_AFTER_CHILD,
                        message: "property declared after a child widget: the engine applies all \
                                  of a widget's properties before creating any of its children, so \
                                  this has no effect on behavior — purely a readability issue"
                            .to_owned(),
                        span: SyntaxTree::span_of(key),
                    });
                }
            }
            _ => {}
        }
    }
}

/// The two style-resolution checks of spec §2.2/§4.2 — [`UNKNOWN_BASE`] and
/// [`UNKNOWN_ROOT_STYLE`] — over `root`'s **top-level** children only.
///
/// Both need to tell "genuinely unresolved" apart from "resolves cross-file", which only the
/// workspace [`WidgetContext`] can answer; with no `ctx` (plain [`analyze`]) this is a no-op, so a
/// catalog-only buffer degrades exactly as before this check existed.
///
/// Only top-level nodes are considered — a nested `Name < Base` or bare tag inside a widget's body
/// is a child-widget *instance*, not a style declaration or the file's root, and is out of scope
/// here (see the [`crate::style_index`] module docs: "only top-level declarations are styles").
fn check_top_level_style_resolution(
    root: Node<'_>,
    source: &str,
    ctx: Option<&WidgetContext>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(ctx) = ctx else { return };
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "style_header" => {
                let Some(base) = child.child_by_field_name("base") else {
                    continue;
                };
                let name = slice(source, base).trim();
                if name.is_empty() || resolves_style(name, ctx) {
                    continue;
                }
                out.push(Diagnostic {
                    severity: Severity::Warning,
                    code: UNKNOWN_BASE,
                    message: format!(
                        "unknown base `{name}`: no workspace style or built-in widget class named `{name}`"
                    ),
                    span: SyntaxTree::span_of(base),
                });
            }
            "container" => {
                let Some(tag) = child.child_by_field_name("tag") else {
                    continue;
                };
                let name = slice(source, tag).trim();
                if name.is_empty() || resolves_style(name, ctx) {
                    continue;
                }
                out.push(Diagnostic {
                    severity: Severity::Error,
                    code: UNKNOWN_ROOT_STYLE,
                    message: format!(
                        "root widget style `{name}` does not resolve: the file fails to load"
                    ),
                    span: SyntaxTree::span_of(tag),
                });
            }
            _ => {}
        }
    }
}

/// Whether `name` resolves to *something* the engine would accept as a widget type: a native `UI*`
/// built-in ([`is_native_base`]), a workspace `.otui` style declaration, or a Lua-declared widget
/// class (`extends`/`onStyleApply`) — the latter covers a custom base whose only declaration is a
/// Lua module, never an `.otui` `Name < Base` line.
fn resolves_style(name: &str, ctx: &WidgetContext) -> bool {
    is_native_base(name) || !ctx.styles.lookup(name).is_empty() || !ctx.lua.lookup(name).is_empty()
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

/// An anchor is only creatable when the **containing** widget uses an anchor layout: the engine
/// throws `"cannot create anchor, the parent widget doesn't use anchor layout!"` otherwise
/// (`uiwidgetbasestyle.cpp`, the `anchors.` dispatch).
///
/// Every widget defaults to `UIAnchorLayout`, so this only bites when the parent *explicitly* switches
/// layout — `layout: horizontalBox` (or `verticalBox` / `grid`) — and a child underneath it still
/// anchors. That is a plausible authoring mistake and, unlike a runtime id lookup, it is fully
/// decidable from the tree: the parent widget and its `layout:` are right there.
///
/// Deliberately conservative — the hint is only raised when the parent's layout type is a **literal**
/// non-anchor type. A `$variable` type, a missing type, an absent `layout:`, or a parent we cannot
/// resolve all mean "assume anchor layout" and stay silent, per the prefer-false-negatives rule.
fn check_anchor_parent_layout(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    let Some(widget) = enclosing_widget_node(node) else {
        return;
    };
    let Some(parent) = enclosing_widget_node(widget) else {
        return; // top-level widget: its runtime parent is unknown, so we cannot judge
    };
    let Some(layout_type) = declared_layout_type(parent, source) else {
        return; // no explicit layout: the default is UIAnchorLayout, which anchors fine
    };
    // `anchor` is the anchor layout; anything else replaces it. An unrecognized type is already an
    // INVALID_PROPERTY_VALUE error from `check_property_value`, so do not pile on here.
    if layout_type == "anchor" || !schema::is_layout_type(&layout_type) {
        return;
    }
    let span = node
        .child_by_field_name("edge")
        .map_or_else(|| SyntaxTree::span_of(node), SyntaxTree::span_of);
    out.push(Diagnostic {
        severity: Severity::Error,
        code: ANCHOR_PARENT_NO_ANCHOR_LAYOUT,
        message: format!(
            "cannot anchor: the parent widget uses `{layout_type}` layout, not an anchor layout"
        ),
        span,
    });
}

/// The nearest **ancestor** widget node of `node` — a `container` or a `style_header` — skipping
/// `node` itself. `None` at the document root.
fn enclosing_widget_node<'t>(node: Node<'t>) -> Option<Node<'t>> {
    let mut current = node.parent()?;
    loop {
        if matches!(current.kind(), "container" | "style_header") {
            return Some(current);
        }
        current = current.parent()?;
    }
}

/// The layout **type** a widget node explicitly declares, as a literal: the value of a leaf
/// `layout: <type>` or of the `type:` key inside a `layout:` block. `None` when the widget declares no
/// `layout:`, or declares one whose type is absent or is a `$variable` (unresolvable statically).
fn declared_layout_type(widget: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = widget.walk();
    let layout = widget
        .named_children(&mut cursor)
        .filter(|c| c.kind() == "property")
        .find(|c| {
            c.child_by_field_name("key")
                .is_some_and(|k| slice(source, k) == "layout")
        })?;

    // Leaf form: `layout: verticalBox`.
    if let Some(value) = layout.child_by_field_name("value") {
        let text = slice(source, value).trim();
        return (!text.is_empty() && !text.starts_with('$')).then(|| text.to_owned());
    }

    // Block form: `layout:` with a nested `type:`. `_block` is hidden, so it is a direct child.
    let mut inner = layout.walk();
    let type_node = layout
        .named_children(&mut inner)
        .filter(|c| c.kind() == "property")
        .find(|c| {
            c.child_by_field_name("key")
                .is_some_and(|k| slice(source, k) == "type")
        })?;
    let value = type_node.child_by_field_name("value")?;
    let text = slice(source, value).trim();
    (!text.is_empty() && !text.starts_with('$')).then(|| text.to_owned())
}

/// Validate an `anchors.<edge>: <target>` node (grammar: `anchor_property` with an `edge` field
/// aliased to `anchor_edge` and an optional `value` field of kind `anchor_target` whose `target`
/// field is a dotted `identifier`). Two edge tokens are checked; the target *id* is intentionally
/// not resolved here (cross-file id existence is a later node).
fn check_anchor_property(node: Node<'_>, source: &str, out: &mut Vec<Diagnostic>) {
    check_anchor_parent_layout(node, source, out);

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
        .any(|child| child.id() != key.id() && value.is_none_or(|v| child.id() != v.id()));
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
    //
    // Checked **before** the `has_block` skip, too. That skip exists because a container-form
    // property is a child-widget group or a list parent, never a leaf style property — but *inside* a
    // `layout:` block there is no such shape: every key a layout reads is a leaf. A block-shaped key
    // there is read by nobody, so it is unknown like any other, and letting `has_block` swallow it
    // would leave a hole in an otherwise exclusive check.
    if enclosing_block_key(node, source) == Some("layout") {
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
    if has_block {
        return;
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

    #[test]
    fn a_compound_state_selector_is_valid() {
        // The engine splits the tag after `$` on SPACES and requires every state to match
        // (`uiwidget.cpp`: `stdext::split(statesStr, " ")`), so `$pressed disabled:` is a
        // conjunction — pressed AND disabled — and each token may carry its own `!`. This shape is
        // all over the engine's own `data/styles/`; the grammar used to reject it as a syntax error.
        for src in [
            "Button\n  $pressed disabled:\n    color: #111111\n",
            "Button\n  $checked hover !disabled:\n    color: #222222\n",
            "Button\n  $first on:\n    color: #333333\n",
        ] {
            assert!(analyze(src).is_empty(), "{src:?} -> {:?}", analyze(src));
        }
    }

    #[test]
    fn the_states_of_a_compound_selector_are_still_validated() {
        // Accepting the shape must not stop us checking the names in it.
        let src = "Button\n  $bogus disabled:\n    color: #111111\n";
        let diags = analyze(src);
        let d = only(&diags, UNKNOWN_STATE);
        assert_eq!(&src[d.span.start..d.span.end], "bogus");
    }

    #[test]
    fn a_style_meta_key_is_not_an_unknown_property() {
        // `__class:` re-roots the widget class and is read by the engine's style manager
        // (`styleNode->valueAt("__class")`); `__unique` likewise. Neither goes through the widget
        // style parser, so neither is in the base catalog — but both are perfectly valid.
        assert!(analyze("SpinBox < TextEdit\n  __class: UISpinBox\n").is_empty());
        assert!(analyze("Win < UIWindow\n  __unique: true\n").is_empty());
    }

    // --- semantic pass: property after child (hint) --------------------------------------------

    #[test]
    fn property_after_a_child_widget_is_a_hint_on_the_key() {
        let src = "\
Panel
  Button
    id: ok
  margin-bottom: 3
";
        let diags = analyze(src);
        let d = only(&diags, PROPERTY_AFTER_CHILD);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "margin-bottom");
    }

    #[test]
    fn property_before_a_child_widget_is_silent() {
        let src = "\
Panel
  margin-bottom: 3
  Button
    id: ok
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn a_bare_tag_at_the_document_root_is_not_a_child_widget() {
        // The document root has no widget body. A root-level `container` is the file's main widget
        // instance, not somebody's child, and a stray root-level property is a different mistake
        // entirely. Emitting the ordering hint here would assert something false about the engine.
        let src = "\
MainWindow
  id: x
stray: value
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn a_tag_inside_a_state_block_is_not_a_child_widget() {
        // `$on:` carries a `:`, so `OTMLParser` marks the node unique (otmlparser.cpp:435) and
        // `createWidgetFromOTML`'s `if (!childNode->isUnique())` loop never descends into it — a tag
        // in there instantiates no widget at all. So a property following it is not "after a child",
        // and the hint's message ("the engine applies all properties before creating any children")
        // would be a false statement about nodes that are not children.
        let src = "\
Panel
  $on:
    Button
    color: red
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn a_child_declared_with_an_explicit_base_also_counts_as_a_child() {
        // A child widget has two spellings: a bare tag (`Button`) and a tag with an inline base
        // (`Label < UILabel`). The grammar gives the second one the `style_header` kind, not
        // `container` — so an implementation that only watches for `container` silently misses
        // every property that follows a based child. It did, until this test.
        let src = "\
MainWindow < UIWindow
  Label < UILabel
    id: title
  margin-bottom: 3
";
        let diags = analyze(src);
        let d = only(&diags, PROPERTY_AFTER_CHILD);
        assert_eq!(d.severity, Severity::Hint);
        assert_eq!(&src[d.span.start..d.span.end], "margin-bottom");
    }

    #[test]
    fn a_top_level_style_declaration_is_not_a_child_widget() {
        // The flip side of the rule above: at the document root, `Name < Base` is a style
        // *declaration* (spec §2.2), not a child of anything. Marking it as "a child was seen"
        // would make every subsequent top-level node look like a misordered property.
        let src = "\
Button < UIButton
  color: red

Label < UILabel
  color: blue
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn a_state_block_after_a_child_widget_is_silent() {
        // The important case: a `$state` selector block is a conditional override that
        // idiomatically reads last, not a readability mistake — it must never trigger the hint,
        // even though it textually follows the child (this is 14/18 of the corpus occurrences).
        let src = "\
Panel
  Button
    id: ok
  $on:
    color: red
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn nested_blocks_reset_the_seen_child_flag_independently() {
        // Both the outer widget (Panel) and the inner one (Button) have a property after their own
        // child — each block tracks its own "seen a child" state, so both fire independently.
        let src = "\
Panel
  Button
    id: ok
    Label
    icon-color: red
  margin-bottom: 3
";
        let diags = analyze(src);
        let flagged: Vec<&str> = diags
            .iter()
            .filter(|d| d.code == PROPERTY_AFTER_CHILD)
            .map(|d| &src[d.span.start..d.span.end])
            .collect();
        assert_eq!(flagged, vec!["icon-color", "margin-bottom"], "{diags:?}");
    }

    #[test]
    fn a_widget_with_no_children_is_silent() {
        let src = "\
Panel
  id: main
  margin-bottom: 3
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
    }

    #[test]
    fn a_malformed_line_never_synthesizes_a_bogus_child_widget() {
        // Regression, found on the real corpus (`data/styles/30-calendar.otui`): a colon typo
        // (`anchors:top:` instead of `anchors.top:`) throws the parser into error recovery, which
        // can synthesize a bogus `container` node out of the wreckage of the rest of the line. That
        // recovery artifact is not a real child widget — every property after it in this block must
        // stay silent, on top of the syntax-error already reported for the malformed line itself.
        let src = "\
Label
  id: day
  anchors:top: parent.top
  height: 15
  text-align: topleft
";
        let diags = analyze(src);
        assert!(
            diags.iter().all(|d| d.code != PROPERTY_AFTER_CHILD),
            "{diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.code == SYNTAX_ERROR),
            "the malformed line itself must still be a syntax error: {diags:?}"
        );
    }

    // --- semantic pass: anchor vs the parent's layout ------------------------------------------

    #[test]
    fn anchoring_under_a_non_anchor_layout_parent_is_an_error() {
        // The engine throws "cannot create anchor, the parent widget doesn't use anchor layout!"
        // while applying the style, so the file fails to load outright.
        let src = "\
Panel
  layout:
    type: horizontalBox

  Button
    anchors.left: parent.left
";
        let diags = analyze(src);
        let d = only(&diags, ANCHOR_PARENT_NO_ANCHOR_LAYOUT);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "left");
    }

    #[test]
    fn anchoring_under_the_leaf_layout_form_is_also_an_error() {
        let src = "Panel\n  layout: verticalBox\n  Button\n    anchors.top: parent.top\n";
        assert_eq!(
            only(&analyze(src), ANCHOR_PARENT_NO_ANCHOR_LAYOUT).severity,
            Severity::Error
        );
    }

    #[test]
    fn anchoring_under_a_default_layout_parent_is_silent() {
        // Every widget defaults to UIAnchorLayout, so a parent with no `layout:` anchors fine.
        let src = "Panel\n  Button\n    anchors.left: parent.left\n";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn anchoring_under_an_explicit_anchor_layout_parent_is_silent() {
        let src = "Panel\n  layout:\n    type: anchor\n  Button\n    anchors.left: parent.left\n";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn a_top_level_widgets_anchors_are_not_judged() {
        // Its runtime parent is unknown (it depends on where the style is instantiated), so we
        // cannot know the parent's layout — stay silent rather than guess.
        let src = "Panel\n  anchors.fill: parent\n";
        assert!(analyze(src).is_empty(), "{:?}", analyze(src));
    }

    #[test]
    fn a_variable_layout_type_does_not_trigger_the_anchor_check() {
        // `$var` resolves at runtime; we cannot know it is non-anchor, so we must not flag.
        let src =
            "Panel\n  layout:\n    type: $myLayout\n  Button\n    anchors.left: parent.left\n";
        assert!(
            analyze(src)
                .iter()
                .all(|d| d.code != ANCHOR_PARENT_NO_ANCHOR_LAYOUT),
            "{:?}",
            analyze(src)
        );
    }

    #[test]
    fn the_anchoring_widgets_own_layout_does_not_matter() {
        // The engine checks the *parent's* layout, not the anchoring widget's own.
        let src = "\
Panel
  Button
    layout:
      type: grid
    anchors.left: parent.left
";
        assert!(
            analyze(src)
                .iter()
                .all(|d| d.code != ANCHOR_PARENT_NO_ANCHOR_LAYOUT),
            "{:?}",
            analyze(src)
        );
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
    fn a_block_shaped_key_inside_a_layout_block_is_still_hinted() {
        // Every key a layout reads is a leaf, so a *container-form* key inside `layout:` is read by
        // nobody. The generic `has_block` skip (which exists for child-widget groups and list
        // parents) must not swallow it, or the layout check stops being exclusive.
        let src = "Panel\n  layout:\n    type: grid\n    bogus:\n      x: 1\n";
        let diags = analyze(src);
        let d = diags
            .iter()
            .find(|d| d.code == UNKNOWN_PROPERTY && &src[d.span.start..d.span.end] == "bogus")
            .unwrap_or_else(|| panic!("`bogus` must be hinted: {diags:?}"));
        assert_eq!(d.severity, Severity::Hint);
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
    fn a_class_reroot_applies_inside_the_declaring_styles_own_body() {
        // `SpinBox < TextEdit` + `__class: UISpinBox` — the engine instantiates a `UISpinBox`, which
        // declares `minimum` in Lua. Seeding the header's body with its *base* (`TextEdit`) misses
        // the re-root, because the re-root lives on `SpinBox`'s own StyleDef — so `minimum:` written
        // right here was hinted, even though the same property on a `SpinBox` *instance* resolved.
        let styles = styles(&[(
            "a.otui",
            "TextEdit < UITextEdit\nSpinBox < TextEdit\n  __class: UISpinBox\n",
        )]);
        let lua = lua(&[(
            "s.lua",
            "UISpinBox = extends(UITextEdit, 'UISpinBox')\n\
             function UISpinBox:onStyleApply(styleName, styleNode)\n\
               for name, value in pairs(styleNode) do\n\
                 if name == 'minimum' then self:setMinimum(value) end\n\
               end\n\
             end\n",
        )]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };

        let src = "SpinBox < TextEdit\n  __class: UISpinBox\n  minimum: 0\n";
        assert!(
            analyze_with_widgets(src, &ctx).is_empty(),
            "{:?}",
            analyze_with_widgets(src, &ctx)
        );

        // Still exclusive: the same property on the *base* style is not valid there.
        let base_src = "TextEdit < UITextEdit\n  minimum: 0\n";
        assert_eq!(
            only(&analyze_with_widgets(base_src, &ctx), UNKNOWN_PROPERTY).severity,
            Severity::Hint
        );
    }

    #[test]
    fn an_unindexed_style_header_still_resolves_through_its_base() {
        // Fallback: when the declared name is not in the style index, the body must still resolve
        // through the base — the behaviour before the name became the preferred seed. Otherwise the
        // degenerate case would regress into *more* hints.
        let styles = styles(&[]); // `Table` is deliberately NOT indexed
        let lua = lua(&[(
            "t.lua",
            "UITable = extends(UIWidget, 'UITable')\n\
             function UITable:onStyleApply(styleName, styleNode)\n\
               for name, value in pairs(styleNode) do\n\
                 if name == 'column-style' then self:setColumnStyle(value) end\n\
               end\n\
             end\n",
        )]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };

        let src = "Table < UITable\n  column-style: SomeColumn\n";
        assert!(
            analyze_with_widgets(src, &ctx).is_empty(),
            "must fall back to the base: {:?}",
            analyze_with_widgets(src, &ctx)
        );
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

    // --- style-resolution: dangling base / dangling root style (spec §2.2, §4.2) ---------------

    #[test]
    fn a_base_that_resolves_in_the_workspace_index_is_silent() {
        let styles = styles(&[("a.otui", "Base < UIWidget\n")]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Derived < Base\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(diags.iter().all(|d| d.code != UNKNOWN_BASE), "{diags:?}");
    }

    #[test]
    fn a_native_ui_prefixed_base_is_never_flagged() {
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "MainWindow < UIWindow\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(diags.iter().all(|d| d.code != UNKNOWN_BASE), "{diags:?}");
    }

    #[test]
    fn a_base_declared_only_in_lua_is_not_flagged() {
        // A widget class whose only declaration is a Lua module (an `extends` line with no
        // `.otui` `Name < Base`, and not a `UI*`-prefixed name) must still resolve.
        let styles = styles(&[]);
        let lua = lua(&[("w.lua", "MyThing = extends(UIWidget, 'MyThing')\n")]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Derived < MyThing\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(diags.iter().all(|d| d.code != UNKNOWN_BASE), "{diags:?}");
    }

    #[test]
    fn a_dangling_base_is_a_warning_on_the_base_token_span() {
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Derived < Nonexistent\n";
        let diags = analyze_with_widgets(src, &ctx);
        let d = only(&diags, UNKNOWN_BASE);
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(&src[d.span.start..d.span.end], "Nonexistent");
    }

    #[test]
    fn a_dangling_base_nested_in_a_widget_body_is_not_flagged() {
        // Only *top-level* `Name < Base` headers are style declarations; a nested one is a
        // child-widget instance, out of scope for this check.
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "MainWindow < UIWindow\n  Inner < NoSuchBase\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(diags.iter().all(|d| d.code != UNKNOWN_BASE), "{diags:?}");
    }

    #[test]
    fn a_style_only_file_with_no_root_instance_is_silent() {
        // Only `Name < Base` declarations, no bare-tag instance node — a valid "style-only" file
        // (spec §4.2): neither check fires, even when the base is dangling (covered separately).
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "MainWindow < UIWindow\nButton < UIButton\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_ROOT_STYLE),
            "{diags:?}"
        );
    }

    #[test]
    fn the_root_instance_resolving_is_silent() {
        let styles = styles(&[("a.otui", "Base < UIWidget\n")]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Base\n  id: main\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_ROOT_STYLE),
            "{diags:?}"
        );
    }

    #[test]
    fn an_unresolved_root_instance_is_an_error_on_the_tag_span() {
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "NoSuchWidget\n  id: main\n";
        let diags = analyze_with_widgets(src, &ctx);
        let d = only(&diags, UNKNOWN_ROOT_STYLE);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(&src[d.span.start..d.span.end], "NoSuchWidget");
    }

    #[test]
    fn a_native_root_instance_is_never_flagged() {
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "UIWidget\n  id: main\n";
        let diags = analyze_with_widgets(src, &ctx);
        assert!(
            diags.iter().all(|d| d.code != UNKNOWN_ROOT_STYLE),
            "{diags:?}"
        );
    }

    #[test]
    fn plain_analyze_without_a_workspace_context_never_produces_style_resolution_diagnostics() {
        // `analyze` (no `ctx`) cannot tell "unresolved" from "resolves cross-file", so it must
        // never invent either diagnostic — behaviour is unchanged from before this check existed.
        let src = "Derived < Nonexistent\nNoSuchWidget\n  id: main\n";
        let diags = analyze(src);
        assert!(
            diags
                .iter()
                .all(|d| d.code != UNKNOWN_BASE && d.code != UNKNOWN_ROOT_STYLE),
            "{diags:?}"
        );
    }

    #[test]
    fn a_dangling_base_on_a_declaration_with_a_body_is_still_flagged() {
        // Every other UNKNOWN_BASE case here is a body-less header, but a real style declaration
        // always has an indented body. The grammar's `_block` is a hidden rule — the body's
        // statements become direct children of the `style_header` itself rather than of a wrapper
        // node — so a header with a body is still a top-level `style_header` and is still scanned.
        // Pinning that: if the body ever started producing a different top-level node kind, the
        // check would silently stop firing on every real-world file and the corpus would read a
        // reassuring zero for the wrong reason.
        let styles = styles(&[]);
        let lua = lua(&[]);
        let ctx = WidgetContext {
            styles: &styles,
            lua: &lua,
        };
        let src = "Derived < Nonexistent\n  id: x\n  color: red\n";
        let diags = analyze_with_widgets(src, &ctx);
        let d = only(&diags, UNKNOWN_BASE);
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(&src[d.span.start..d.span.end], "Nonexistent");
    }
}
