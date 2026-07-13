//! Hover descriptions for style tokens (spec §5.5): the engine's structured answer to
//! "what does the token under the cursor mean?".
//!
//! Unlike the pure locators in [`navigation`](crate::navigation), this module **resolves** the
//! located token against the workspace [`StyleIndex`](crate::style_index::StyleIndex) and makes every
//! language decision — native `UI*` base vs. user base, whether a base resolves in the workspace, how
//! many definitions it has, and what it inherits. It returns a protocol-agnostic [`StyleHover`]
//! (byte-offset span + a structured [`StyleHoverKind`]); turning that into Markdown / an LSP `Hover`
//! is the server's job. Keeping the semantics here (not in the transport crate) is the same rule the
//! rest of the engine follows: `otui-core` decides meaning, the server only formats it.
//!
//! No I/O, no `lsp-types`.

use crate::navigation::style_header_at;
use crate::style_index::{StyleIndex, is_native_base};
use lang_api::ByteSpan;

/// The base a style inherits from, resolved for hover display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inheritance {
    /// The base's name (the `Base` in `Name < Base`).
    pub base: String,
    /// Whether that base is a native `UI*` built-in class (vs. a user style).
    pub native: bool,
}

/// A structured, protocol-agnostic description of what a hover over a style token conveys (spec §5.5).
///
/// The engine makes every language decision in [`kind`](Self::kind); the server only formats those
/// facts into Markdown and maps [`span`](Self::span) — the exact token the cursor is on — to a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleHover {
    /// The byte span of the token the cursor is on (the declared name or the base), for highlighting.
    pub span: ByteSpan,
    /// What that token means.
    pub kind: StyleHoverKind,
}

/// The meaning of the hovered style token — one variant per thing a hover can describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleHoverKind {
    /// The cursor is on a native `UI*` base: a built-in widget class with no defining `.otui` file.
    NativeBase {
        /// The base name (e.g. `UIWindow`).
        name: String,
    },
    /// The cursor is on a user base that resolves to `def_count` workspace definition(s) (duplicates
    /// are legal). When the chosen definition inherits further, that next hop is in `inherits`.
    UserBase {
        /// The base name.
        name: String,
        /// How many workspace definitions declare this name (always ≥ 1 here).
        def_count: usize,
        /// The base the resolved definition itself inherits from, if any.
        inherits: Option<Inheritance>,
    },
    /// The cursor is on a user base declared nowhere in the workspace (a dangling reference).
    DanglingBase {
        /// The base name.
        name: String,
    },
    /// The cursor is on a style's declared name; `inherits` is this style's own base, if any.
    StyleName {
        /// The declared style name.
        name: String,
        /// The base this style inherits from, if the header carries one.
        inherits: Option<Inheritance>,
    },
}

/// Describe what a hover at `offset` conveys, resolving against the workspace `index` (spec §5.5).
///
/// Returns `None` when the cursor is not on a top-level style header's declared-name or base token
/// (delegating that decision to [`style_header_at`]). The result is **deterministic**: when a base
/// resolves to several definitions (legal — duplicates are allowed), they are ordered by
/// `(document id, name-span start)` before the inherited-base next hop is chosen, so the same inputs
/// always produce the same description.
#[must_use]
pub fn style_hover_at(source: &str, offset: usize, index: &StyleIndex) -> Option<StyleHover> {
    let header = style_header_at(source, offset)?;

    // Base branch: the cursor is within the base token — describe the base.
    if let (Some(base), Some(base_span)) = (header.base.as_deref(), header.base_span)
        && base_span.start <= offset
        && offset < base_span.end
    {
        return Some(StyleHover {
            span: base_span,
            kind: classify_base(base, index),
        });
    }

    // Name branch: describe this style and, if present, what it inherits from.
    let inherits = header.base.as_deref().map(inheritance_of);
    Some(StyleHover {
        span: header.name_span,
        kind: StyleHoverKind::StyleName {
            name: header.name,
            inherits,
        },
    })
}

/// Classify a `Name < Base` base name against the workspace index (base branch of [`style_hover_at`]).
fn classify_base(base: &str, index: &StyleIndex) -> StyleHoverKind {
    if is_native_base(base) {
        return StyleHoverKind::NativeBase {
            name: base.to_owned(),
        };
    }
    let mut hits = index.lookup(base);
    if hits.is_empty() {
        return StyleHoverKind::DanglingBase {
            name: base.to_owned(),
        };
    }
    // Order the definitions deterministically so the chosen next-hop base never varies between runs
    // when the same name is declared in several documents.
    hits.sort_by(|(a_doc, a_def), (b_doc, b_def)| {
        a_doc
            .as_str()
            .cmp(b_doc.as_str())
            .then(a_def.name_span.start.cmp(&b_def.name_span.start))
    });
    let inherits = hits
        .iter()
        .find_map(|(_, def)| def.base.as_deref())
        .map(inheritance_of);
    StyleHoverKind::UserBase {
        name: base.to_owned(),
        def_count: hits.len(),
        inherits,
    }
}

/// Build the [`Inheritance`] descriptor for a base name, classifying its native-ness up front.
fn inheritance_of(base: &str) -> Inheritance {
    Inheritance {
        base: base.to_owned(),
        native: is_native_base(base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style_index::{DocId, StyleIndex, extract_style_defs};
    use crate::syntax::SyntaxTree;

    /// Build a workspace index from `(doc_id, source)` entries, indexing each document's style defs.
    fn index_of(entries: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (id, src) in entries {
            let tree = SyntaxTree::parse(src).expect("parses");
            index.set_document(DocId::from(*id), extract_style_defs(&tree));
        }
        index
    }

    /// Byte offset of the first occurrence of `needle` in `src`.
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    #[test]
    fn native_base_is_classified_as_built_in() {
        let index = index_of(&[("file:///a.otui", "MyPanel < UIWidget\n")]);
        let src = "MyPanel < UIWidget\n";
        let hover = style_hover_at(src, at(src, "UIWidget"), &index).expect("hit");
        assert_eq!(
            hover.kind,
            StyleHoverKind::NativeBase {
                name: "UIWidget".to_owned()
            }
        );
        // The span is the base token.
        assert_eq!(&src[hover.span.start..hover.span.end], "UIWidget");
    }

    #[test]
    fn user_base_resolves_with_its_own_inheritance() {
        let index = index_of(&[
            ("file:///defs.otui", "MyPanel < UIWidget\n"),
            ("file:///use.otui", "Child < MyPanel\n"),
        ]);
        let src = "Child < MyPanel\n";
        let hover = style_hover_at(src, at(src, "MyPanel"), &index).expect("hit");
        assert_eq!(
            hover.kind,
            StyleHoverKind::UserBase {
                name: "MyPanel".to_owned(),
                def_count: 1,
                inherits: Some(Inheritance {
                    base: "UIWidget".to_owned(),
                    native: true,
                }),
            }
        );
    }

    #[test]
    fn dangling_base_has_no_definition() {
        let index = index_of(&[("file:///a.otui", "Child < Missing\n")]);
        let src = "Child < Missing\n";
        let hover = style_hover_at(src, at(src, "Missing"), &index).expect("hit");
        assert_eq!(
            hover.kind,
            StyleHoverKind::DanglingBase {
                name: "Missing".to_owned()
            }
        );
    }

    #[test]
    fn duplicate_definitions_report_their_count_deterministically() {
        // The same base name declared in two files, each inheriting from a different native class.
        // The chosen next-hop base must be stable across runs (ordered by doc id, then span).
        let index = index_of(&[
            ("file:///b.otui", "Dup < UIWindow\n"),
            ("file:///a.otui", "Dup < UIWidget\n"),
        ]);
        let src = "Child < Dup\n";
        let hover = style_hover_at(src, at(src, "Dup"), &index).expect("hit");
        match hover.kind {
            StyleHoverKind::UserBase {
                name,
                def_count,
                inherits,
            } => {
                assert_eq!(name, "Dup");
                assert_eq!(def_count, 2);
                // `file:///a.otui` sorts before `file:///b.otui`, so its base (`UIWidget`) wins.
                assert_eq!(
                    inherits,
                    Some(Inheritance {
                        base: "UIWidget".to_owned(),
                        native: true,
                    })
                );
            }
            other => panic!("expected UserBase, got {other:?}"),
        }
    }

    #[test]
    fn declared_name_describes_the_style_and_its_base() {
        let index = index_of(&[("file:///a.otui", "MainWindow < UIWindow\n")]);
        let src = "MainWindow < UIWindow\n";
        let hover = style_hover_at(src, at(src, "MainWindow"), &index).expect("hit");
        assert_eq!(
            hover.kind,
            StyleHoverKind::StyleName {
                name: "MainWindow".to_owned(),
                inherits: Some(Inheritance {
                    base: "UIWindow".to_owned(),
                    native: true,
                }),
            }
        );
        assert_eq!(&src[hover.span.start..hover.span.end], "MainWindow");
    }

    #[test]
    fn bare_header_name_has_no_inheritance() {
        let index = index_of(&[("file:///a.otui", "Standalone\n  id: x\n")]);
        let src = "Standalone\n  id: x\n";
        let hover = style_hover_at(src, at(src, "Standalone"), &index).expect("hit");
        assert_eq!(
            hover.kind,
            StyleHoverKind::StyleName {
                name: "Standalone".to_owned(),
                inherits: None,
            }
        );
    }

    #[test]
    fn non_header_offset_yields_nothing() {
        let index = index_of(&[("file:///a.otui", "MainWindow < UIWindow\n  id: main\n")]);
        let src = "MainWindow < UIWindow\n  id: main\n";
        assert!(style_hover_at(src, at(src, "main"), &index).is_none());
    }
}
