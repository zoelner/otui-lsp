//! Hover Markdown for an `id:` **declaration** value (spec §5.5): "this widget's id" plus a reference
//! count.
//!
//! Complements [`property_hover`](crate::property_hover) (property-key hover) and
//! [`hover`](crate::hover) (style-name/base hover) for the third spec §5.5 row. This module only
//! formats: the server counts the id's occurrences — the document-local anchor targets
//! ([`references::id_occurrences`](crate::references::id_occurrences), mirroring
//! `include_declaration: false`) plus the paired `.lua` controller's uses (a server-side concern,
//! since it needs the workspace's Lua-ref index) — and calls [`id_hover_body`] with the two counts.
//!
//! ## Engine ground (spec §2.3)
//!
//! `UIWidget::setId` (`uiwidget.cpp:1057-1073`) makes an id a key on the **parent**'s
//! `m_childrenById` map, and a Lua field on the parent. It is **not** uniqueness-enforced:
//! * the map assignment (`uiwidget.cpp:1066`) is a plain `operator[]=` — a duplicate sibling id is
//!   **last-writer-wins** for `getChildById`/anchor-target lookups (`uiwidget.cpp:201,266`);
//! * the Lua field assignment (`uiwidget.cpp:1057-1073`, guarded at `:222-227`) only sets the field if
//!   it is not already present — a duplicate sibling id is **first-writer-wins** for `widget.someId`
//!   dotted Lua access.
//!
//! So the hover copy must never claim the id is unique — a real corpus sample reuses an id value
//! within one file at a non-trivial rate. [`id_hover_body`]'s `has_duplicate_decl` flag exists purely
//! to surface that caveat when it applies.
//!
//! Pure: no I/O, no `lsp-types` — a `String` in, a `String` (Markdown) out. The server wraps it in an
//! LSP `Hover` with a range over the declaration token's span.

/// Build the full Markdown `Hover` **value** for an `id:` declaration (spec §5.5): the header
/// (`` **`<id>`** — this widget's id. ``), a reference-count line, an optional
/// otui/Lua breakdown, and an optional not-unique caveat.
///
/// * `anchor_count` — how many `<id>.edge` anchor targets in **this document** reference `id`
///   (mirrors `references::id_occurrences(..).anchor_refs.len()`, which already excludes the
///   declaration itself — `include_declaration: false`).
/// * `lua_count` — how many uses of `id` the paired `.lua` controller(s) contain
///   (`Backend::lua_forward_references`, the identical call the `references` handler uses, so this
///   hover's count is always equal to what a `textDocument/references` request on the same token
///   would return).
/// * `has_duplicate_decl` — whether `id` is declared more than once in this document
///   (`IdOccurrences::has_duplicate_declaration`); when true, a caveat is appended so the hover never
///   implies the id is unique (see the module doc comment's engine-ground note).
///
/// The count line:
/// * `0` → `No references.`
/// * `1` → `1 reference.`
/// * `n` → `{n} references.`, with a parenthetical breakdown appended (before the final period) when
///   **both** `anchor_count` and `lua_count` are nonzero, e.g. `3 references (2 anchors in this file,
///   1 in the paired Lua controller(s)).`
#[must_use]
pub fn id_hover_body(
    id: &str,
    anchor_count: usize,
    lua_count: usize,
    has_duplicate_decl: bool,
) -> String {
    let mut value = format!("**`{id}`** — this widget's id.");

    let total = anchor_count + lua_count;
    let mut count_line = match total {
        0 => "No references".to_owned(),
        1 => "1 reference".to_owned(),
        n => format!("{n} references"),
    };
    if anchor_count > 0 && lua_count > 0 {
        let anchor_word = if anchor_count == 1 {
            "anchor"
        } else {
            "anchors"
        };
        count_line.push_str(&format!(
            " ({anchor_count} {anchor_word} in this file, {lua_count} in the paired Lua controller(s))"
        ));
    }
    count_line.push('.');

    value.push_str("\n\n");
    value.push_str(&count_line);

    if has_duplicate_decl {
        value.push_str("\n\nNote: `");
        value.push_str(id);
        value.push_str(
            "` is declared more than once in this document. OTClient does not enforce unique ids: \
             a duplicate sibling id is last-writer-wins for `getChildById`/anchor-target lookups, but \
             first-writer-wins for dotted Lua field access (`UIWidget::setId`).",
        );
    }

    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_names_the_id_and_calls_it_this_widgets_id() {
        let body = id_hover_body("header", 0, 0, false);
        assert!(
            body.starts_with("**`header`** — this widget's id."),
            "{body}"
        );
    }

    #[test]
    fn zero_references_says_no_references() {
        let body = id_hover_body("solo", 0, 0, false);
        assert!(body.contains("No references."), "{body}");
    }

    #[test]
    fn one_reference_is_singular() {
        let body = id_hover_body("header", 1, 0, false);
        assert!(body.contains("1 reference."), "{body}");
        assert!(!body.contains("1 references"), "{body}");
    }

    #[test]
    fn one_lua_reference_alone_is_also_singular() {
        let body = id_hover_body("header", 0, 1, false);
        assert!(body.contains("1 reference."), "{body}");
    }

    #[test]
    fn many_references_are_plural() {
        let body = id_hover_body("header", 3, 0, false);
        assert!(body.contains("3 references."), "{body}");
    }

    #[test]
    fn otui_only_references_have_no_breakdown() {
        // Only one of the two counts is nonzero: no parenthetical breakdown, just the plain count.
        let body = id_hover_body("header", 2, 0, false);
        assert!(body.contains("2 references."), "{body}");
        assert!(!body.contains("("), "{body}");
    }

    #[test]
    fn lua_only_references_have_no_breakdown() {
        let body = id_hover_body("header", 0, 2, false);
        assert!(body.contains("2 references."), "{body}");
        assert!(!body.contains("("), "{body}");
    }

    #[test]
    fn mixed_references_get_a_breakdown() {
        // The exact phrasing spec'd: "3 references (2 anchors in this file, 1 in the paired Lua controller(s))."
        let body = id_hover_body("header", 2, 1, false);
        assert!(
            body.contains(
                "3 references (2 anchors in this file, 1 in the paired Lua controller(s))."
            ),
            "{body}"
        );
    }

    #[test]
    fn a_single_anchor_in_the_breakdown_is_singular() {
        let body = id_hover_body("header", 1, 1, false);
        assert!(
            body.contains(
                "2 references (1 anchor in this file, 1 in the paired Lua controller(s))."
            ),
            "{body}"
        );
    }

    #[test]
    fn no_duplicate_caveat_when_declared_once() {
        let body = id_hover_body("header", 0, 0, false);
        assert!(
            !body.to_lowercase().contains("declared more than once"),
            "{body}"
        );
    }

    #[test]
    fn duplicate_declaration_adds_a_not_unique_caveat() {
        let body = id_hover_body("header", 0, 0, true);
        assert!(body.contains("declared more than once"), "{body}");
        // Must never claim the id is unique — it must instead explain the engine's actual
        // last-writer-wins (lookup) / first-writer-wins (Lua field) behavior.
        assert!(body.contains("last-writer-wins"), "{body}");
        assert!(body.contains("first-writer-wins"), "{body}");
        assert!(!body.to_lowercase().contains("must be unique"), "{body}");
    }

    #[test]
    fn duplicate_caveat_coexists_with_a_populated_reference_count() {
        let body = id_hover_body("dup", 2, 1, true);
        assert!(
            body.contains(
                "3 references (2 anchors in this file, 1 in the paired Lua controller(s))."
            ),
            "{body}"
        );
        assert!(body.contains("declared more than once"), "{body}");
    }
}
