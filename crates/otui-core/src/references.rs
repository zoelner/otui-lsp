//! Reference occurrence-finders (spec §5.4): the per-document half of `textDocument/references`.
//!
//! Given a target symbol name, these functions scan one parsed document and report **where** that
//! symbol occurs — as byte spans, with no resolution and no `lsp-types`. The server drives them:
//! it classifies what the cursor is on (via [`navigation`](crate::navigation)), then calls the
//! matching finder over each open document and maps the spans to LSP `Location`s.
//!
//! ## Two namespaces, two scopes
//!
//! * **Style names** live in the global workspace namespace (spec §5.2). A name `N` occurs as a
//!   top-level declaration `N < …` and as a base `X < N`. [`style_name_occurrences`] finds both in
//!   one document; the server fans it out across every open document (the namespace is global).
//! * **`id:` values** are per-widget-tree identities (spec §2.3), referenced by anchor targets
//!   `<id>.edge` (spec §2.4). [`id_occurrences`] finds an id's declaration and its anchor references
//!   **within a single document**. Ids can repeat across files/widgets, so cross-document id
//!   references are ambiguous — the server keeps them document-local (only the current document).
//!
//! ## Fidelity notes
//!
//! * **Exact, case-sensitive name match.** The engine's style map (`UIManager::m_styles`) and its id
//!   lookups compare names by exact string equality — mirroring [`StyleIndex::lookup`] and
//!   [`is_native_base`](crate::style_index::is_native_base), which never fold case. These finders do
//!   the same (`==`), so `Panel` and `panel` are different symbols.
//! * **Only top-level `Name < Base` headers are style occurrences.** A `Name < Base` nested inside a
//!   widget block is an instance, not an inheritance declaration; only the document root's direct
//!   children are scanned, matching [`extract_style_defs`](crate::style_index::extract_style_defs)
//!   and [`base_reference_at`](crate::navigation::base_reference_at).
//!
//! Everything here is byte-offset based. No I/O, no `lsp-types`.

use crate::syntax::SyntaxTree;
use lang_api::ByteSpan;
use tree_sitter::Node;

/// Where a style name `N` occurs within one document (spec §5.4).
///
/// [`declarations`](Self::declarations) are the name spans of top-level `N < …` headers (the
/// definition sites — the server includes them only when the request's `context.include_declaration`
/// is set). [`base_refs`](Self::base_refs) are the base spans of top-level `X < N` headers (the usage
/// sites). Both are byte spans into the document this was scanned from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleNameOccurrences {
    /// Spans of the declared-name token in every top-level `N < …` declaration.
    pub declarations: Vec<ByteSpan>,
    /// Spans of the base token in every top-level `X < N` base reference.
    pub base_refs: Vec<ByteSpan>,
}

/// Find every occurrence of the style name `name` in `source` (spec §5.4).
///
/// Scans only the document root's direct children: a top-level `style_header` contributes a
/// [`declaration`](StyleNameOccurrences::declarations) when its declared name equals `name`, and a
/// [`base_ref`](StyleNameOccurrences::base_refs) when its base equals `name`. The same header can be
/// both (a `Name < Name` self-reference). Matching is exact and case-sensitive. Returns empty
/// occurrences when the source cannot be parsed.
#[must_use]
pub fn style_name_occurrences(source: &str, name: &str) -> StyleNameOccurrences {
    let mut out = StyleNameOccurrences::default();
    let Some(tree) = SyntaxTree::parse(source) else {
        return out;
    };
    let root = tree.root();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "style_header" {
            continue;
        }
        if let Some(node) = child.child_by_field_name("name")
            && slice(source, node) == name
        {
            out.declarations.push(SyntaxTree::span_of(node));
        }
        if let Some(node) = child.child_by_field_name("base")
            && slice(source, node) == name
        {
            out.base_refs.push(SyntaxTree::span_of(node));
        }
    }
    out
}

/// Where an `id:` value occurs within one document (spec §5.4).
///
/// [`declaration`](Self::declaration) is the span of the `id:` value token (the definition site — the
/// server includes it only when `context.include_declaration` is set); if the same id is declared on
/// several widgets in the document, the **first** in source order is kept (ambiguous by construction,
/// so a single declaration is reported). [`anchor_refs`](Self::anchor_refs) are the spans of the `id`
/// portion of every `<id>.edge` anchor target that references it. All spans are byte spans into the
/// document this was scanned from; id references are document-local.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdOccurrences {
    /// The span of the `id:` value token, if the id is declared in this document.
    pub declaration: Option<ByteSpan>,
    /// The spans of the `id` prefix in every `<id>.edge` anchor target referencing it.
    pub anchor_refs: Vec<ByteSpan>,
}

/// Find every occurrence of the id `id` in `source` (spec §5.4), scanning the whole document.
///
/// An `id_property` whose value equals `id` is the [`declaration`](IdOccurrences::declaration) (first
/// in source order wins). An `anchor_target` of the dotted form `<id>.edge` whose `id` prefix equals
/// `id` is an [`anchor reference`](IdOccurrences::anchor_refs) — its span covers just the `id` prefix,
/// not the `.edge` suffix. A bare anchor target (a magic keyword such as `parent`/`prev`/`next`, with
/// no dot) never references an id. Matching is exact and case-sensitive. Returns empty occurrences
/// when the source cannot be parsed.
#[must_use]
pub fn id_occurrences(source: &str, id: &str) -> IdOccurrences {
    let mut out = IdOccurrences::default();
    let Some(tree) = SyntaxTree::parse(source) else {
        return out;
    };
    collect_id_occurrences(tree.root(), source, id, &mut out);
    out
}

/// Depth-first walk collecting id declarations and anchor references (ids nest at any depth).
fn collect_id_occurrences(node: Node<'_>, source: &str, id: &str, out: &mut IdOccurrences) {
    match node.kind() {
        "id_property" => {
            if let Some(value) = node.child_by_field_name("value")
                && out.declaration.is_none()
                && slice(source, value) == id
            {
                out.declaration = Some(SyntaxTree::span_of(value));
            }
        }
        "anchor_target" => {
            if let Some(prefix) = anchor_id_prefix(node, source, id) {
                out.anchor_refs.push(prefix);
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_id_occurrences(child, source, id, out);
    }
}

/// If `anchor_target` is a dotted `<id>.edge` whose `id` prefix equals `id`, return the span of that
/// prefix; otherwise `None`. A bare (dot-less) target is a magic keyword and never an id reference.
fn anchor_id_prefix(anchor_target: Node<'_>, source: &str, id: &str) -> Option<ByteSpan> {
    let target = anchor_target.child_by_field_name("target")?;
    let span = SyntaxTree::span_of(target);
    let text = &source[span.start..span.end];
    let dot = text.find('.')?; // no dot → magic keyword, not an `<id>.edge` reference
    let prefix = &text[..dot];
    // A dotted magic target (`parent.bottom`, `next.top`, `prev.left`) references the magic widget,
    // not a user id — its prefix is never an id reference even if it happens to equal `id`.
    if crate::schema::is_magic_anchor_target(prefix) {
        return None;
    }
    if prefix == id {
        Some(ByteSpan::new(span.start, span.start + dot))
    } else {
        None
    }
}

/// Slice `source` by `node`'s byte span.
fn slice<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    &source[node.start_byte()..node.end_byte()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The substrings `source` covers for each span in `spans`, for readable assertions.
    fn texts<'a>(source: &'a str, spans: &[ByteSpan]) -> Vec<&'a str> {
        spans.iter().map(|s| &source[s.start..s.end]).collect()
    }

    #[test]
    fn style_name_found_as_declaration_and_as_base() {
        // `Base` is declared once and used as a base once, in the same document.
        let src = "Base < UIWidget\nChild < Base\n";
        let occ = style_name_occurrences(src, "Base");
        assert_eq!(texts(src, &occ.declarations), ["Base"]);
        assert_eq!(texts(src, &occ.base_refs), ["Base"]);
        // The declaration span is the *first* `Base` (the declared name), the base ref the second.
        assert_eq!(occ.declarations[0].start, src.find("Base").unwrap());
        assert_eq!(occ.base_refs[0].start, src.rfind("Base").unwrap());
    }

    #[test]
    fn style_name_with_no_other_uses_yields_only_its_declaration() {
        let src = "Lonely < UIWidget\n";
        let occ = style_name_occurrences(src, "Lonely");
        assert_eq!(texts(src, &occ.declarations), ["Lonely"]);
        assert!(occ.base_refs.is_empty());
    }

    #[test]
    fn style_name_match_is_exact_and_case_sensitive() {
        let src = "Panel < UIWidget\nOther < panel\n";
        // `panel` (lowercase) must not match `Panel`.
        let occ = style_name_occurrences(src, "Panel");
        assert_eq!(texts(src, &occ.declarations), ["Panel"]);
        assert!(
            occ.base_refs.is_empty(),
            "lowercase `panel` is a different symbol"
        );
        // ...and querying the lowercase form finds only the base ref.
        let occ = style_name_occurrences(src, "panel");
        assert!(occ.declarations.is_empty());
        assert_eq!(texts(src, &occ.base_refs), ["panel"]);
    }

    #[test]
    fn nested_headers_are_not_style_occurrences() {
        // The nested `Inner < Base` is a widget instance, not a top-level declaration/base ref.
        let src = "Outer < UIWidget\n  Inner < Base\nBase < UIWidget\n";
        let occ = style_name_occurrences(src, "Base");
        // Only the top-level `Base < UIWidget` declaration is found; the nested base is ignored.
        assert_eq!(occ.declarations.len(), 1);
        assert!(occ.base_refs.is_empty());
    }

    #[test]
    fn style_name_absent_yields_nothing() {
        let src = "Panel < UIWidget\n";
        let occ = style_name_occurrences(src, "Missing");
        assert!(occ.declarations.is_empty());
        assert!(occ.base_refs.is_empty());
    }

    #[test]
    fn id_declaration_and_anchor_references_in_one_document() {
        let src = "Panel\n  id: header\nOther\n  anchors.top: header.bottom\n  anchors.left: header.left\n";
        let occ = id_occurrences(src, "header");
        // The declaration is the `id:` value `header`.
        let decl = occ.declaration.expect("declared");
        assert_eq!(&src[decl.start..decl.end], "header");
        assert_eq!(decl.start, src.find("header").unwrap());
        // Two anchor targets reference it; each span covers just the `header` prefix, not `.edge`.
        assert_eq!(texts(src, &occ.anchor_refs), ["header", "header"]);
        for span in &occ.anchor_refs {
            assert_eq!(&src[span.start..span.end], "header");
        }
    }

    #[test]
    fn id_with_no_references_yields_only_its_declaration() {
        let src = "Panel\n  id: solo\n";
        let occ = id_occurrences(src, "solo");
        assert!(occ.declaration.is_some());
        assert!(occ.anchor_refs.is_empty());
    }

    #[test]
    fn bare_magic_anchor_target_is_not_an_id_reference() {
        // `parent` / `none` are magic keywords (no dot); they must not be id references even when an
        // id happens to share their spelling.
        let src = "Panel\n  id: parent\n  anchors.fill: parent\n  anchors.top: none\n";
        let occ = id_occurrences(src, "parent");
        assert!(
            occ.declaration.is_some(),
            "the `id: parent` declaration is still found"
        );
        assert!(
            occ.anchor_refs.is_empty(),
            "bare `parent`/`none` targets have no dot, so reference no id"
        );
    }

    #[test]
    fn dotted_magic_anchor_target_is_not_an_id_reference() {
        // `parent.bottom` references the magic parent widget, not an id named `parent`, so querying
        // references for the id `parent` must not collect it (only the real `id: parent` decl).
        let src = "Panel\n  id: parent\n  anchors.top: parent.bottom\n";
        let occ = id_occurrences(src, "parent");
        assert!(occ.declaration.is_some(), "the `id: parent` decl is found");
        assert!(
            occ.anchor_refs.is_empty(),
            "a dotted magic target's prefix is not an id reference"
        );
    }

    #[test]
    fn id_match_is_exact_and_case_sensitive() {
        let src = "Panel\n  id: Header\nOther\n  anchors.top: header.bottom\n";
        // `Header` (declaration) and `header` (anchor prefix) are different ids.
        let occ = id_occurrences(src, "Header");
        assert!(occ.declaration.is_some());
        assert!(occ.anchor_refs.is_empty());
        let occ = id_occurrences(src, "header");
        assert!(occ.declaration.is_none());
        assert_eq!(texts(src, &occ.anchor_refs), ["header"]);
    }

    #[test]
    fn first_id_declaration_wins_when_repeated() {
        // Two widgets declare `id: dup`; only the first (source order) is reported as the declaration.
        let src = "A\n  id: dup\nB\n  id: dup\n";
        let occ = id_occurrences(src, "dup");
        let decl = occ.declaration.expect("declared");
        assert_eq!(
            decl.start,
            src.find("dup").unwrap(),
            "the first declaration wins"
        );
    }

    #[test]
    fn absent_id_yields_nothing() {
        let src = "Panel\n  id: header\n";
        let occ = id_occurrences(src, "missing");
        assert!(occ.declaration.is_none());
        assert!(occ.anchor_refs.is_empty());
    }
}
