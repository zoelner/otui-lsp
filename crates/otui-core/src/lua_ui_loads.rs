//! Indexing where OTClient **Lua** code loads a `.otui` file at runtime — the other half of the
//! module-association mechanism this crate needs to pair a controller with its UI beyond
//! [`lua_refs`](crate::lua_refs)'s same-directory/same-stem fast path.
//!
//! OTClient's own module loader (`Module::parse`, `src/framework/core/module.cpp`) resolves a
//! module's `.otui` file(s) not from any naming convention, but from the literal string argument a
//! controller passes to one of three UI-manager calls:
//!
//! ```lua
//! g_ui.loadUI('styles/wheelMenu', mainPanel)  -- form 1: load, attach to `mainPanel`
//! g_ui.displayUI('battle')                    -- form 2: load and show
//! g_ui.importStyle('style.otui')              -- form 3: import style declarations only
//! ```
//!
//! Every one of these resolves its (non-`/`-rooted) argument **relative to the directory of the
//! `.lua` file that is calling it** (`LuaInterface::loadScript`'s `getCurrentSourcePath()`, which
//! walks the Lua call stack for the nearest function's own source path) — with `.otui` implied when
//! the argument carries no extension. That is only the *module's* own directory for a top-level
//! controller; a controller living in a subdirectory (`game_cyclopedia/tab/bestiary/bestiary.lua`,
//! real corpus example, calling `g_ui.loadUI("bestiary")`) resolves against ITS OWN directory
//! (`tab/bestiary/`, where the sibling `bestiary.otui` actually lives), not the module root. A
//! leading `/` instead resolves against the mounted virtual filesystem root — rare in the real
//! corpus (a handful of complete-literal calls) and left unresolved by the server rather than
//! guessed at (see `otui-lsp-server`'s `scan_module_dir`). [`scan_ui_loads`] finds every
//! complete-literal call of the three forms; turning the returned (still-relative) path into an
//! absolute file — and deciding whether that file actually exists — is server-side work (this crate
//! does no I/O), done by `otui-lsp-server`'s module-association index alongside
//! [`crate::otmod::otmod_scripts`] (which finds a module's controllers in the first place).
//!
//! ## Corpus-derived rules — this is what shapes the scan
//!
//! Measured against the real OTClient engine tree (`otclient`):
//!
//! * **Only a string literal that is the COMPLETE first argument counts**, exactly like
//!   [`lua_refs`](crate::lua_refs)'s `getChildById` rule — with one difference: **a second argument
//!   is allowed and ignored**. All three calls commonly take a `parent`/`options` second argument
//!   (`g_ui.loadUI('styles/controls/general', controller.ui.optionsTabContent)`), so the scan accepts
//!   either `)` or `,` immediately after the first argument's closing quote, not only `)` — unlike
//!   `getChildById`, which never takes a second argument in the engine. A call built from a variable
//!   or a concatenation (`g_ui.loadUI('/' .. self.name .. '/' .. source, parent)`, real code in
//!   `modules/modulelib/controller.lua`) still yields nothing: the id is not known at scan time, so
//!   it can never be navigated or diagnosed.
//! * **The argument may be a bare name or a path.** `'battle'` names `battle.otui` next to the
//!   controller; `'styles/wheelMenu'` and `'style/changeListName'` name a file in a subdirectory. The
//!   scan does not interpret the string at all — it hands back exactly what was written, verbatim,
//!   leaving path-joining and `.otui`-extension inference to the caller (which also needs the
//!   module's directory, something this crate never has).
//! * **Comments and unrelated strings must never contribute a load.** Reuses
//!   [`lua_refs::excluded_ranges`](crate::lua_refs::excluded_ranges) verbatim rather than
//!   re-deriving the same comment/long-string/short-string exclusion pass — the two modules scan the
//!   exact same Lua-as-text surface, and a second, independently-evolving copy of "what counts as a
//!   comment or a string here" is exactly the kind of drift [`lua_refs`](crate::lua_refs) warns about
//!   in its own doc comment.
//!
//! ## Heuristic parse (no Lua grammar)
//!
//! Same discipline as [`lua_refs`](crate::lua_refs): byte-oriented, deliberately conservative, no Lua
//! grammar in this workspace.

use crate::lua_refs::{
    excluded_ranges, in_excluded, is_ident_boundary_after, is_ident_boundary_before,
    leading_string_literal,
};
use lang_api::ByteSpan;

/// Which of the three UI-load call forms a [`UiLoadRef`] was found as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiLoadKind {
    /// `g_ui.loadUI('name'[, parent])` — load a `.otui` file, optionally attaching the result under
    /// `parent`.
    LoadUi,
    /// `g_ui.displayUI('name')` — load a `.otui` file and show it.
    DisplayUi,
    /// `g_ui.importStyle('name')` — import a `.otui` file's style declarations only (no widget tree
    /// is created).
    ImportStyle,
}

/// One place in a Lua source that loads a `.otui` file by name.
///
/// `path` is exactly the string literal's content, unresolved — still relative to the calling
/// module's own directory, and possibly missing its `.otui` extension (see the module doc comment).
/// `span` covers the path token itself — the text inside the quotes — not the surrounding call, so a
/// consumer can turn it directly into a `Location` landing the cursor on the name, exactly like
/// [`crate::lua_refs::LuaIdRef::span`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiLoadRef {
    pub path: String,
    pub span: ByteSpan,
    pub kind: UiLoadKind,
}

/// Find every `g_ui.loadUI`/`g_ui.displayUI`/`g_ui.importStyle` call in `source` whose first
/// argument is a complete string literal (module doc comment). Comments and unrelated string
/// literals are excluded first, exactly like [`crate::lua_refs::scan_id_refs`]. The returned refs
/// are ordered by their span's start offset.
#[must_use]
pub fn scan_ui_loads(source: &str) -> Vec<UiLoadRef> {
    let excluded = excluded_ranges(source);
    let mut out: Vec<UiLoadRef> = call_first_string_literal(source, &excluded, "g_ui.loadUI")
        .map(|(path, span)| UiLoadRef {
            path,
            span,
            kind: UiLoadKind::LoadUi,
        })
        .chain(
            call_first_string_literal(source, &excluded, "g_ui.displayUI").map(|(path, span)| {
                UiLoadRef {
                    path,
                    span,
                    kind: UiLoadKind::DisplayUi,
                }
            }),
        )
        .chain(
            call_first_string_literal(source, &excluded, "g_ui.importStyle").map(|(path, span)| {
                UiLoadRef {
                    path,
                    span,
                    kind: UiLoadKind::ImportStyle,
                }
            }),
        )
        .collect();
    out.sort_by_key(|r| r.span.start);
    out
}

/// Every whole-word call to `name` in `source` whose **first** argument is a complete string
/// literal, as `(literal, content_span)` — the immediate following byte (after optional whitespace)
/// must be `)` (a sole argument) or `,` (a second argument follows, ignored). A call whose name
/// occurs inside a comment or string (per `excluded`), whose first argument is not a complete
/// literal (a variable, a concatenation), or whose literal is empty, contributes nothing.
///
/// Deliberately not [`crate::lua_refs`]'s `call_string_literals`: that one requires the literal to be
/// the call's *sole* argument, which `getChildById`/`setId` always are in the engine but
/// `loadUI`/`displayUI`/`importStyle` are not (a `parent` second argument is routine — see the module
/// doc comment).
fn call_first_string_literal<'a>(
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
        let rest = &source[args_start..];
        let (literal, content_start, content_end, after_offset) = leading_string_literal(rest)?;
        if literal.is_empty() {
            return None;
        }
        let after = rest[after_offset..].trim_start();
        if !(after.starts_with(')') || after.starts_with(',')) {
            return None;
        }
        Some((
            literal,
            ByteSpan::new(args_start + content_start, args_start + content_end),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(src: &str, span: ByteSpan) -> &str {
        &src[span.start..span.end]
    }

    #[test]
    fn load_ui_with_a_bare_name_is_indexed() {
        let src = "wheelWindow = g_ui.loadUI('wheel')\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, UiLoadKind::LoadUi);
        assert_eq!(refs[0].path, "wheel");
        assert_eq!(text(src, refs[0].span), "wheel");
    }

    #[test]
    fn load_ui_with_a_path_argument_is_indexed() {
        let src = "wheelOfDestinyWindow = g_ui.loadUI('styles/wheelMenu', mainPanel)\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "styles/wheelMenu");
        assert_eq!(text(src, refs[0].span), "styles/wheelMenu");
    }

    #[test]
    fn display_ui_is_indexed() {
        let src = "serverListWindow = g_ui.displayUI('serverlist')\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, UiLoadKind::DisplayUi);
        assert_eq!(refs[0].path, "serverlist");
    }

    #[test]
    fn import_style_is_indexed() {
        let src = "g_ui.importStyle(\"styles/style.otui\")\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, UiLoadKind::ImportStyle);
        assert_eq!(refs[0].path, "styles/style.otui");
    }

    #[test]
    fn a_second_argument_does_not_suppress_the_first() {
        // Real code (`client_options/options.lua`): a `parent` widget is a routine second argument,
        // unlike anything `getChildById`/`setId` ever take.
        let src = "panels.generalPanel = g_ui.loadUI('styles/controls/general', \
                   controller.ui.optionsTabContent)\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "styles/controls/general");
    }

    #[test]
    fn a_concatenated_argument_is_not_indexed() {
        // Real code (`modules/modulelib/controller.lua`): the path is built at runtime, so it can
        // never be navigated or diagnosed.
        let src = "ui = g_ui.loadUI('/' .. self.name .. '/' .. source, parent or rootWidget)\n";
        assert!(
            scan_ui_loads(src).is_empty(),
            "a concatenation-built path must never be indexed"
        );
    }

    #[test]
    fn a_variable_argument_is_not_indexed() {
        let src = "actionBars[i] = g_ui.loadUI(layout, parent)\n";
        assert!(scan_ui_loads(src).is_empty());
    }

    #[test]
    fn a_load_inside_a_line_comment_is_not_indexed() {
        let src = "-- g_ui.loadUI('ghost')\n";
        assert!(scan_ui_loads(src).is_empty());
    }

    #[test]
    fn a_load_inside_a_block_comment_is_not_indexed() {
        let src = "--[[\ng_ui.loadUI('ghost')\n]]\nlocal x = 1\n";
        assert!(scan_ui_loads(src).is_empty());
    }

    #[test]
    fn a_load_inside_a_string_is_not_indexed() {
        let src = "local code = [[ g_ui.loadUI('ghost') ]]\ng_ui.loadUI('real')\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "real");
    }

    #[test]
    fn both_quote_styles_are_recognized() {
        let single = scan_ui_loads("g_ui.loadUI('single')\n");
        assert_eq!(single[0].path, "single");
        let double = scan_ui_loads("g_ui.loadUI(\"double\")\n");
        assert_eq!(double[0].path, "double");
    }

    #[test]
    fn all_three_forms_in_one_file_are_all_found_in_span_order() {
        let src = "g_ui.importStyle('style.otui')\ng_ui.loadUI('a')\ng_ui.displayUI('b')\n";
        let refs = scan_ui_loads(src);
        let kinds: Vec<UiLoadKind> = refs.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                UiLoadKind::ImportStyle,
                UiLoadKind::LoadUi,
                UiLoadKind::DisplayUi
            ]
        );
        let paths: Vec<&str> = refs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["style.otui", "a", "b"]);
    }

    #[test]
    fn a_name_with_g_ui_load_ui_only_as_a_substring_is_not_matched() {
        // The whole-word boundary check must apply to the full multi-segment name, not just its
        // first character: `my_g_ui.loadUI(...)` contains the literal substring `g_ui.loadUI`, but
        // the byte immediately before it (`_`) is an identifier character, so this is not a call to
        // the real `g_ui` global at all.
        let src = "local x = my_g_ui.loadUI('not-real')\n";
        assert!(scan_ui_loads(src).is_empty());
    }
}
