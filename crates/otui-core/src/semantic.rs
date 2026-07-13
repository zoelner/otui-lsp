//! Semantic tokens (spec §3 token taxonomy): a leaf-level highlight over the CST.
//!
//! This pass walks the tree-sitter [`SyntaxTree`] and maps meaningful **leaf** grammar nodes to a
//! protocol-agnostic [`SemanticTokenKind`]. It emits one token per leaf and never for a container
//! node, so the output is naturally non-overlapping; it is then sorted by span start and any
//! residual overlaps are dropped deterministically, satisfying the LSP requirement that semantic
//! tokens be sorted and non-overlapping.
//!
//! Node-kind → [`SemanticTokenKind`] mapping (see [`kind_for`]):
//!
//! | grammar node kind                                                    | token kind   |
//! |----------------------------------------------------------------------|--------------|
//! | `comment`                                                            | `Comment`    |
//! | `style_name`, `tag`                                                  | `Type`       |
//! | `style_base` recognised by [`is_native_base`](crate::style_index::is_native_base)     | `BuiltinType` |
//! | `style_base` not recognised by `is_native_base`                      | `InheritedType` |
//! | `event_name`                                                         | `Event`      |
//! | `property_key`, `id_key`, `anchor_keyword`, `anchor_edge`, `alias_name`, `expr_name` | `Property` |
//! | `string`, `hash_literal`, `color`                                    | `String`     |
//! | `number`                                                             | `Number`     |
//! | `boolean`                                                            | `Boolean`    |
//! | `state_name` in the known set                                        | `EnumMember` |
//! | `state_name` outside the known set                                   | `UnknownState` |
//! | `variable`                                                           | `Variable`   |
//! | `state_negation` (the `!`)                                           | `Operator`   |
//! | `null` (the `~`)                                                     | `Keyword`    |
//! | `plain_value` under `id_property`                                    | `Variable`   |
//! | `plain_value` elsewhere                                              | `String`     |
//! | `identifier` under a non-`fill`/`centerIn` `anchor_target`, leading segment `parent`/`prev`/`next` | `Keyword` (leading segment) + `Variable` (`.edge`, if any) |
//! | `identifier` under a non-`fill`/`centerIn` `anchor_target`, otherwise | `Variable` (whole target) |
//! | `identifier` under a `fill`/`centerIn` `anchor_target`, whole text exactly `parent`/`prev`/`next` | `Keyword` (whole target) |
//! | `identifier` under a `fill`/`centerIn` `anchor_target`, otherwise     | `Variable` (whole target, never split) |
//! | `identifier` elsewhere (inline-array word)                           | `String`     |
//!
//! Deliberately **not** tokenized: structural punctuation (`<`, `:`, `$`, `.`, `[`, `]`, `,`, `-`,
//! `@`, `&`) is anonymous and skipped to keep the highlight minimal; `lua_value`,
//! `block_scalar_marker` and `block_scalar_content` are the raw bodies reserved for the future
//! embedded-Lua injection bridge and are left untouched (a Lua semantic pass will own them).
//! `color` is classed as `String` (not `Number`) so the whole `#rrggbb` / `rgba(...)` literal
//! reads as one atom rather than a number with punctuation.
//!
//! An `anchor_target` (the `<target>` in `anchors.<edge>: <target>`) is a single `DOTTED` grammar
//! token, but the engine treats it differently depending on the edge. For an ordinary edge (`left`,
//! `right`, `top`, `bottom`, `horizontalCenter`, `verticalCenter`), the runtime splits the target on
//! `.` into a widget id and a hooked edge (`src/framework/ui/uiwidgetbasestyle.cpp`), and
//! `UIAnchor::getHookedWidget` (`src/framework/ui/uianchorlayout.cpp`) treats exactly `parent`,
//! `prev` and `next` as the leading widget id as the relative hooked widget; any other leading
//! segment is a concrete widget id (`getChildById`). [`collect`] mirrors that split, splitting the
//! one grammar token into up to two semantic tokens: the magic leading segment as `Keyword` and the
//! remaining `.edge` part as `Variable`. A concrete id (including one that merely contains a magic
//! word, e.g. `parentPanel`) stays a single `Variable` token, matching an ordinary id reference.
//!
//! `fill` and `centerIn` are different: the engine passes the **whole, unsplit** target string
//! straight to `UIWidget::fill`/`centerIn` (`src/framework/ui/uiwidgetbasestyle.cpp`), which forward
//! it to `getHookedWidget` unchanged — so the magic check there compares the *entire* string, not a
//! dot-separated leading segment. `anchors.fill: parent` is the magic parent reference (`Keyword`,
//! the whole token), but `anchors.fill: parent.left` is a single concrete widget id lookup for
//! `getChildById("parent.left")` (`Variable`, the whole token, never split on `.`). [`collect`]
//! (via [`emit_anchor_target`]) reads the sibling `edge` field to tell the two cases apart.

use crate::schema;
use crate::style_index::is_native_base;
use crate::syntax::SyntaxTree;
use lang_api::{ByteSpan, SemanticToken, SemanticTokenKind};
use tree_sitter::Node;

/// Compute leaf-level, sorted, non-overlapping semantic tokens for `source`.
#[must_use]
pub fn tokens(source: &str) -> Vec<SemanticToken> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };

    let mut raw = Vec::new();
    collect(tree.root(), source, &mut raw);

    // LSP requires tokens sorted by start and non-overlapping. Leaves already don't overlap, but
    // sort defensively and drop any token that starts before the previous kept token ends.
    raw.sort_by_key(|t| (t.span.start, t.span.end));

    let mut out: Vec<SemanticToken> = Vec::with_capacity(raw.len());
    let mut last_end = 0usize;
    for tok in raw {
        if tok.span.is_empty() || tok.span.start < last_end {
            continue;
        }
        last_end = tok.span.end;
        out.push(tok);
    }
    out
}

/// The [`SemanticTokenKind`] for `node`, or `None` if this node kind is not tokenized.
///
/// A `plain_value` is context-sensitive: a `Variable` when it is an `id:` value (the id being
/// defined) and a `String` otherwise. An `anchor_target` identifier is handled separately by
/// [`emit_anchor_target`] (called from [`collect`]), since it may expand into more than one
/// token; the `identifier` arm here only ever fires for a bare inline-array word.
fn kind_for(node: Node<'_>, source: &str) -> Option<SemanticTokenKind> {
    use SemanticTokenKind::*;
    let text = || node.utf8_text(source.as_bytes()).unwrap_or_default();
    let kind = match node.kind() {
        "comment" => Comment,
        "style_name" | "tag" => Type,
        // A `< Base` recognised as a native class by `is_native_base` (UI + an uppercase third
        // char, e.g. `UIWidget`) is a built-in; anything else is a file-defined parent style. This
        // is the same classifier `hover`/`widget_resolve`/`completion`/`diagnostics` use, so e.g.
        // `UIwidget` or `UI2Panel` — which merely start with `UI` but aren't the engine's naming
        // convention — are `InheritedType` here too, not `BuiltinType`.
        "style_base" => {
            if is_native_base(text()) {
                BuiltinType
            } else {
                InheritedType
            }
        }
        "event_name" => Event,
        "property_key" | "id_key" | "anchor_keyword" | "anchor_edge" | "alias_name"
        | "expr_name" => Property,
        "string" | "hash_literal" | "color" => String,
        "number" => Number,
        "boolean" => Boolean,
        // A recognised engine state vs one outside the closed 14-name set (which silently never
        // matches at runtime) — the same distinction the state-name hint diagnostic makes.
        "state_name" => {
            if schema::is_known_state(text()) {
                EnumMember
            } else {
                UnknownState
            }
        }
        "variable" => Variable,
        "state_negation" => Operator,
        "null" => Keyword,
        "plain_value" => match node.parent().map(|p| p.kind()) {
            Some("id_property") => Variable,
            _ => String,
        },
        // `anchor_target` identifiers get special multi-token handling in `collect` (see
        // `emit_anchor_target`); this arm is only reached for a bare inline-array word.
        "identifier" => String,
        _ => return None,
    };
    Some(kind)
}

/// Emit the token(s) for an `anchor_target`'s `identifier` node.
///
/// The grammar lexes the whole `<target>` in `anchors.<edge>: <target>` as one `DOTTED` token
/// (e.g. `parent.left` or `closeButton.left`). How the engine reads it depends on the sibling
/// `edge` (read from the enclosing `anchor_property` node):
///
/// - For `fill`/`centerIn`, `UIWidget::fill`/`centerIn` (`src/framework/ui/uiwidgetbasestyle.cpp`)
///   forward the **whole, unsplit** target string to `getHookedWidget`, which compares it against
///   `parent`/`next`/`prev` as a whole. So the target is exactly one token: `Keyword` only when the
///   entire text is one of those three words, `Variable` (unsplit, even if it contains a `.`)
///   otherwise.
/// - For every other edge, the runtime splits the target on `.` into a widget id and a hooked edge
///   first, and `UIAnchor::getHookedWidget` (`src/framework/ui/uianchorlayout.cpp`) treats exactly
///   the leading segments `parent`, `next` and `prev` as the relative hooked widget; any other
///   leading segment is a concrete widget id resolved via `getChildById`. So when the leading
///   dotted segment is exactly one of those three magic words, split the single grammar token into
///   a `Keyword` covering the magic segment and a `Variable` covering the remaining `.edge` part
///   (if any); otherwise emit the whole target as one `Variable` token, unchanged from a plain id
///   reference.
///
/// Both branches match exactly (never a substring), so a concrete id that merely contains a magic
/// word (e.g. `parentPanel`, or `parent.left` under `fill`) is never split.
fn emit_anchor_target(node: Node<'_>, source: &str, out: &mut Vec<SemanticToken>) {
    let span = SyntaxTree::span_of(node);
    let text = node.utf8_text(source.as_bytes()).unwrap_or_default();

    let edge = node
        .parent() // anchor_target
        .and_then(|target| target.parent()) // anchor_property
        .and_then(|property| property.child_by_field_name("edge"))
        .and_then(|edge| edge.utf8_text(source.as_bytes()).ok());
    let whole_string_magic_check = matches!(edge, Some("fill") | Some("centerIn"));

    if whole_string_magic_check {
        let kind = if matches!(text, "parent" | "prev" | "next") {
            SemanticTokenKind::Keyword
        } else {
            SemanticTokenKind::Variable
        };
        out.push(SemanticToken { span, kind });
        return;
    }

    let leading = text.split('.').next().unwrap_or(text);
    if !matches!(leading, "parent" | "prev" | "next") {
        out.push(SemanticToken {
            span,
            kind: SemanticTokenKind::Variable,
        });
        return;
    }
    let leading_end = span.start + leading.len();
    out.push(SemanticToken {
        span: ByteSpan::new(span.start, leading_end),
        kind: SemanticTokenKind::Keyword,
    });
    // `leading_end` is the magic word's end; skip the `.` separator (if any) to reach the edge.
    // A bare `parent` (no `.edge`) has `leading_end == span.end`, so this is skipped entirely.
    let edge_start = leading_end + 1;
    if edge_start < span.end {
        out.push(SemanticToken {
            span: ByteSpan::new(edge_start, span.end),
            kind: SemanticTokenKind::Variable,
        });
    }
}

/// Depth-first walk emitting a token for every mapped leaf. Mapped nodes are all token (leaf)
/// nodes, so recursing into children after emitting can never produce a nested/overlapping token.
///
/// One node kind gets special handling instead of the generic `kind_for` map: an `identifier`
/// under `anchor_target` may expand into up to two tokens (see `emit_anchor_target`); it is a leaf
/// node, so the subsequent recursion into its (nonexistent) children is a no-op.
fn collect(node: Node<'_>, source: &str, out: &mut Vec<SemanticToken>) {
    let is_anchor_target =
        node.kind() == "identifier" && node.parent().map(|p| p.kind()) == Some("anchor_target");
    if is_anchor_target {
        emit_anchor_target(node, source, out);
    } else if let Some(kind) = kind_for(node, source) {
        out.push(SemanticToken {
            span: SyntaxTree::span_of(node),
            kind,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_api::ByteSpan;

    /// Find the token whose slice equals `text`, asserting there is exactly one.
    fn token_for<'a>(src: &str, toks: &'a [SemanticToken], text: &str) -> &'a SemanticToken {
        let matches: Vec<&SemanticToken> = toks
            .iter()
            .filter(|t| &src[t.span.start..t.span.end] == text)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one token for {text:?}, found {matches:?}"
        );
        matches[0]
    }

    const SNIPPET: &str = "\
// header comment
MainWindow < UIWindow
  id: main
  color: #ff0000
  width: 100
  visible: true
  anchors.left: parent.top
  $on:
    color: red
  items: [a, 1, \"x\"]
";

    #[test]
    fn maps_representative_nodes_to_expected_kinds() {
        let toks = tokens(SNIPPET);

        assert_eq!(
            token_for(SNIPPET, &toks, "// header comment").kind,
            SemanticTokenKind::Comment
        );
        // The style name is a Type; a `UI`-prefixed base is a built-in widget class.
        assert_eq!(
            token_for(SNIPPET, &toks, "MainWindow").kind,
            SemanticTokenKind::Type
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "UIWindow").kind,
            SemanticTokenKind::BuiltinType
        );
        // A property key.
        assert_eq!(
            token_for(SNIPPET, &toks, "width").kind,
            SemanticTokenKind::Property
        );
        // The id key is a property; its value is a variable (the id being defined).
        assert_eq!(
            token_for(SNIPPET, &toks, "id").kind,
            SemanticTokenKind::Property
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "main").kind,
            SemanticTokenKind::Variable
        );
        // A color literal (classed as a string) and a number.
        assert_eq!(
            token_for(SNIPPET, &toks, "#ff0000").kind,
            SemanticTokenKind::String
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "100").kind,
            SemanticTokenKind::Number
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "true").kind,
            SemanticTokenKind::Boolean
        );
        // Anchor key parts and the anchor target.
        assert_eq!(
            token_for(SNIPPET, &toks, "anchors").kind,
            SemanticTokenKind::Property
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "left").kind,
            SemanticTokenKind::Property
        );
        // `anchors.left: parent.top` — a magic relative anchor target splits into a `Keyword`
        // (the `parent` reference) and a `Variable` (the `.top` edge); see the dedicated
        // `anchor_target` tests below for the full matrix.
        assert_eq!(
            token_for(SNIPPET, &toks, "parent").kind,
            SemanticTokenKind::Keyword
        );
        assert_eq!(
            token_for(SNIPPET, &toks, "top").kind,
            SemanticTokenKind::Variable
        );
        // A $state name.
        assert_eq!(
            token_for(SNIPPET, &toks, "on").kind,
            SemanticTokenKind::EnumMember
        );
        // A quoted string inside an inline array.
        assert_eq!(
            token_for(SNIPPET, &toks, "\"x\"").kind,
            SemanticTokenKind::String
        );
    }

    #[test]
    fn distinguishes_builtin_and_inherited_bases_events_and_unknown_states() {
        let src = "\
MyButton < UIButton
  @onClick: doThing()
Derived < BaseThing
  $hover:
    color: red
  $wat:
    color: blue
";
        let toks = tokens(src);
        // A `UI`-prefixed base is a built-in widget class; a file-defined base is an inherited type.
        assert_eq!(
            token_for(src, &toks, "UIButton").kind,
            SemanticTokenKind::BuiltinType
        );
        assert_eq!(
            token_for(src, &toks, "BaseThing").kind,
            SemanticTokenKind::InheritedType
        );
        // An `@event` key is its own kind, not a generic property.
        assert_eq!(
            token_for(src, &toks, "onClick").kind,
            SemanticTokenKind::Event
        );
        // A recognised state vs one outside the closed set.
        assert_eq!(
            token_for(src, &toks, "hover").kind,
            SemanticTokenKind::EnumMember
        );
        assert_eq!(
            token_for(src, &toks, "wat").kind,
            SemanticTokenKind::UnknownState
        );
    }

    #[test]
    fn native_base_split_uses_the_canonical_is_native_base_classifier_not_starts_with() {
        // `is_native_base` (`crate::style_index`) requires `UI` followed by an ASCII-uppercase
        // third char. `UIwidget` and `UI2Panel` both start with "UI" (the old, wrong rule) but are
        // NOT native by the canonical classifier, so they must be `InheritedType`, matching
        // `hover`/`widget_resolve`/`completion`/`diagnostics`. Revert-confirm: under the old
        // `text().starts_with("UI")` rule both would wrongly come out `BuiltinType`.
        let src = "\
Foo < UIwidget
Bar < UI2Panel
Baz < UIButton
";
        let toks = tokens(src);
        assert_eq!(
            token_for(src, &toks, "UIwidget").kind,
            SemanticTokenKind::InheritedType
        );
        assert_eq!(
            token_for(src, &toks, "UI2Panel").kind,
            SemanticTokenKind::InheritedType
        );
        // Sanity: a genuine native base is unaffected.
        assert_eq!(
            token_for(src, &toks, "UIButton").kind,
            SemanticTokenKind::BuiltinType
        );
    }

    #[test]
    fn tokens_are_sorted_and_non_overlapping() {
        let toks = tokens(SNIPPET);
        assert!(!toks.is_empty());
        let mut prev_end = 0usize;
        for (i, t) in toks.iter().enumerate() {
            assert!(!t.span.is_empty(), "token {i} is empty");
            assert!(
                t.span.start >= prev_end,
                "token {i} at {:?} overlaps previous end {prev_end}",
                t.span
            );
            prev_end = t.span.end;
        }
    }

    #[test]
    fn variable_reference_and_null_and_negated_state() {
        let src = "\
Panel
  $!on:
    text: ~
  color: $accent
";
        let toks = tokens(src);
        assert_eq!(
            token_for(src, &toks, "$accent").kind,
            SemanticTokenKind::Variable
        );
        assert_eq!(token_for(src, &toks, "~").kind, SemanticTokenKind::Keyword);
        assert_eq!(token_for(src, &toks, "!").kind, SemanticTokenKind::Operator);
        assert_eq!(
            token_for(src, &toks, "on").kind,
            SemanticTokenKind::EnumMember
        );
    }

    #[test]
    fn event_key_is_an_event_alias_and_expr_keys_are_properties() {
        // An `@event:` key is its own `Event` kind; `&alias:` / `!expr:` key names stay `Property`,
        // same as a generic `property_key` — only their Lua-bearing values differ (and those values
        // are untokenized `lua_value`/`hash_literal` bodies, not exercised here).
        let src = "\
Button
  @onClick: g_game.talk(1, 2)
  &primaryColor: #33AAFF
  !text: tr('Label')
";
        let toks = tokens(src);
        assert_eq!(
            token_for(src, &toks, "onClick").kind,
            SemanticTokenKind::Event
        );
        assert_eq!(
            token_for(src, &toks, "primaryColor").kind,
            SemanticTokenKind::Property
        );
        assert_eq!(
            token_for(src, &toks, "text").kind,
            SemanticTokenKind::Property
        );
    }

    #[test]
    fn empty_source_yields_no_tokens() {
        assert_eq!(tokens(""), Vec::<SemanticToken>::new());
    }

    #[test]
    fn plain_value_outside_id_is_a_string() {
        let src = "Panel\n  text: Hello World\n";
        let toks = tokens(src);
        let t = token_for(src, &toks, "Hello World");
        assert_eq!(t.kind, SemanticTokenKind::String);
        assert_eq!(t.span, ByteSpan::new(14, 25));
    }

    // --- anchor_target magic-reference split (parent/prev/next vs. a concrete widget id) -------
    //
    // Engine: `UIAnchor::getHookedWidget` (src/framework/ui/uianchorlayout.cpp) treats exactly
    // the strings "parent", "next" and "prev" as the relative hooked widget; any other target
    // string is resolved via `getChildById` as a concrete widget id.

    #[test]
    fn anchor_target_magic_parent_splits_into_keyword_and_edge_variable() {
        let src = "Panel\n  anchors.left: parent.left\n";
        let toks = tokens(src);
        let target_start = src.find("parent.left").unwrap();

        let keyword_span = ByteSpan::new(target_start, target_start + "parent".len());
        let keyword_tok = toks.iter().find(|t| t.span == keyword_span);
        assert_eq!(
            keyword_tok.map(|t| t.kind),
            Some(SemanticTokenKind::Keyword)
        );

        let edge_start = target_start + "parent.".len();
        let edge_span = ByteSpan::new(edge_start, edge_start + "left".len());
        let edge_tok = toks.iter().find(|t| t.span == edge_span);
        assert_eq!(edge_tok.map(|t| t.kind), Some(SemanticTokenKind::Variable));

        // No stray token re-merges the two halves (e.g. spanning the whole `parent.left` or the
        // `.` separator).
        let whole_span = ByteSpan::new(target_start, target_start + "parent.left".len());
        assert!(!toks.iter().any(|t| t.span == whole_span));
    }

    #[test]
    fn anchor_target_magic_prev_is_a_keyword() {
        let src = "Panel\n  anchors.top: prev.bottom\n";
        let toks = tokens(src);
        let target_start = src.find("prev.bottom").unwrap();
        let keyword_span = ByteSpan::new(target_start, target_start + "prev".len());
        let tok = toks.iter().find(|t| t.span == keyword_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Keyword));
    }

    #[test]
    fn anchor_target_bare_magic_word_is_a_single_keyword_token_with_no_stray_edge() {
        // `anchors.fill: parent` has no `.edge` part; the whole (single-segment) target is one
        // Keyword token and no stray Variable token is emitted for a nonexistent edge.
        let src = "Panel\n  anchors.fill: parent\n";
        let toks = tokens(src);
        let target_start = src.rfind("parent").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "parent".len());

        let anchor_target_toks: Vec<_> = toks
            .iter()
            .filter(|t| t.span.start >= target_start)
            .collect();
        assert_eq!(
            anchor_target_toks.len(),
            1,
            "expected exactly one token for the bare anchor target, found {anchor_target_toks:?}"
        );
        assert_eq!(anchor_target_toks[0].span, target_span);
        assert_eq!(anchor_target_toks[0].kind, SemanticTokenKind::Keyword);
    }

    #[test]
    fn anchor_target_concrete_id_stays_a_single_variable_token() {
        let src = "Panel\n  anchors.left: closeButton.left\n";
        let toks = tokens(src);
        let target_start = src.find("closeButton.left").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "closeButton.left".len());
        let tok = toks.iter().find(|t| t.span == target_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Variable));
        assert!(!toks.iter().any(|t| t.kind == SemanticTokenKind::Keyword));
    }

    // --- fill/centerIn targets: engine passes the whole string, unsplit, to getHookedWidget -------
    //
    // Engine: `UIWidget::fill`/`centerIn` (`src/framework/ui/uiwidgetbasestyle.cpp`) forward the
    // *entire* target string to `getHookedWidget`, which only recognises it as magic when the whole
    // string equals "parent"/"prev"/"next" — unlike a regular edge, it is never `.`-split first.

    #[test]
    fn anchor_target_fill_bare_parent_is_a_single_keyword_token() {
        let src = "Panel\n  anchors.fill: parent\n";
        let toks = tokens(src);
        let target_start = src.rfind("parent").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "parent".len());
        let matching: Vec<_> = toks
            .iter()
            .filter(|t| t.span.start >= target_start)
            .collect();
        assert_eq!(matching.len(), 1, "expected one token, found {matching:?}");
        assert_eq!(matching[0].span, target_span);
        assert_eq!(matching[0].kind, SemanticTokenKind::Keyword);
    }

    #[test]
    fn anchor_target_fill_dotted_parent_is_not_split_and_is_a_single_variable() {
        // Unlike a regular edge, `fill` never splits on `.`: the engine passes "parent.left"
        // whole to `getHookedWidget`, which does not match "parent" exactly, so this is a single
        // concrete `getChildById("parent.left")` lookup, not the magic parent reference.
        // Revert-confirm: under the old always-split-on-first-`.` rule this would wrongly emit a
        // `Keyword` for "parent" plus a `Variable` for "left".
        let src = "Panel\n  anchors.fill: parent.left\n";
        let toks = tokens(src);
        let target_start = src.find("parent.left").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "parent.left".len());
        let tok = toks.iter().find(|t| t.span == target_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Variable));
        assert!(!toks.iter().any(|t| t.kind == SemanticTokenKind::Keyword));
    }

    #[test]
    fn anchor_target_centerin_bare_next_is_a_single_keyword_token() {
        let src = "Panel\n  anchors.centerIn: next\n";
        let toks = tokens(src);
        let target_start = src.rfind("next").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "next".len());
        let tok = toks.iter().find(|t| t.span == target_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Keyword));
    }

    #[test]
    fn anchor_target_centerin_concrete_id_stays_a_single_variable_token() {
        let src = "Panel\n  anchors.centerIn: closeButton\n";
        let toks = tokens(src);
        let target_start = src.find("closeButton").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "closeButton".len());
        let tok = toks.iter().find(|t| t.span == target_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Variable));
        assert!(!toks.iter().any(|t| t.kind == SemanticTokenKind::Keyword));
    }

    #[test]
    fn anchor_target_substring_of_magic_word_is_not_split() {
        // `parentPanel` merely starts with "parent" as a substring; leading-segment matching is
        // exact, so this must NOT be treated as the magic reference (it's a concrete widget id).
        let src = "Panel\n  anchors.left: parentPanel.left\n";
        let toks = tokens(src);
        let target_start = src.find("parentPanel.left").unwrap();
        let target_span = ByteSpan::new(target_start, target_start + "parentPanel.left".len());
        let tok = toks.iter().find(|t| t.span == target_span);
        assert_eq!(tok.map(|t| t.kind), Some(SemanticTokenKind::Variable));
        assert!(!toks.iter().any(|t| t.kind == SemanticTokenKind::Keyword));
    }
}
