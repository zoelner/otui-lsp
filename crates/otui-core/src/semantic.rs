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
//! | `style_base` beginning with `UI`                                     | `BuiltinType` |
//! | `style_base` not beginning with `UI`                                 | `InheritedType` |
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
//! | `identifier` under `anchor_target`                                   | `Variable`   |
//! | `identifier` elsewhere (inline-array word)                           | `String`     |
//!
//! Deliberately **not** tokenized: structural punctuation (`<`, `:`, `$`, `.`, `[`, `]`, `,`, `-`,
//! `@`, `&`) is anonymous and skipped to keep the highlight minimal; `lua_value`,
//! `block_scalar_marker` and `block_scalar_content` are the raw bodies reserved for the future
//! embedded-Lua injection bridge and are left untouched (a Lua semantic pass will own them).
//! `color` is classed as `String` (not `Number`) so the whole `#rrggbb` / `rgba(...)` literal
//! reads as one atom rather than a number with punctuation.

use crate::schema;
use crate::syntax::SyntaxTree;
use lang_api::{SemanticToken, SemanticTokenKind};
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
/// A couple of kinds are context-sensitive: a `plain_value` is a `Variable` when it is an `id:`
/// value (the id being defined) and a `String` otherwise, and an `identifier` is a `Variable` when
/// it is an anchor target and a `String` (a bare inline-array word) otherwise.
fn kind_for(node: Node<'_>, source: &str) -> Option<SemanticTokenKind> {
    use SemanticTokenKind::*;
    let text = || node.utf8_text(source.as_bytes()).unwrap_or_default();
    let kind = match node.kind() {
        "comment" => Comment,
        "style_name" | "tag" => Type,
        // A `< Base` beginning with `UI` names a built-in native widget class; anything else is a
        // file-defined parent style. Same `^UI` split the grammar and diagnostics use.
        "style_base" => {
            if text().starts_with("UI") {
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
        "identifier" => match node.parent().map(|p| p.kind()) {
            Some("anchor_target") => Variable,
            _ => String,
        },
        _ => return None,
    };
    Some(kind)
}

/// Depth-first walk emitting a token for every mapped leaf. Mapped nodes are all token (leaf)
/// nodes, so recursing into children after emitting can never produce a nested/overlapping token.
fn collect(node: Node<'_>, source: &str, out: &mut Vec<SemanticToken>) {
    if let Some(kind) = kind_for(node, source) {
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
  anchors.left: parent.left
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
        assert_eq!(
            token_for(SNIPPET, &toks, "parent.left").kind,
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
}
