//! Indexing where OTClient **Lua** code loads a `.otui` file at runtime — the other half of the
//! module-association mechanism this crate needs to pair a controller with its UI beyond
//! [`lua_refs`](crate::lua_refs)'s same-directory/same-stem fast path.
//!
//! OTClient's own module loader (`Module::parse`, `src/framework/core/module.cpp`) resolves a
//! module's `.otui` file(s) not from any naming convention, but from the literal string argument a
//! controller passes to one of three UI-manager calls (plus one indirect fourth, `setUI`, described
//! below):
//!
//! ```lua
//! g_ui.loadUI('styles/wheelMenu', mainPanel)  -- form 1: load, attach to `mainPanel`
//! g_ui.displayUI('battle')                    -- form 2: load and show
//! g_ui.importStyle('style.otui')              -- form 3: import style declarations only
//! self:setUI('options')                       -- form 4: `Controller:setUI`, see below
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
//! complete-literal call of all four forms; turning the returned (still-relative) path into an
//! absolute file — and deciding whether that file actually exists — is server-side work (this crate
//! does no I/O), done by `otui-lsp-server`'s module-association index alongside
//! [`crate::otmod::otmod_scripts`] (which finds a module's controllers in the first place).
//!
//! ## `Controller:setUI` — a fourth, indirect form
//!
//! `modules/modulelib/controller.lua` layers a `Controller:setUI(name, parent)` method
//! (controller.lua:344-346) on top of `g_ui.loadUI`: it merely records
//! `self.dataUI = { name = name, parent = parent }`; the actual `g_ui.loadUI` call happens later,
//! inside `Controller:loadUI` (controller.lua:337), as
//! `g_ui.loadUI('/' .. self.name .. '/' .. self.dataUI.name, ...)` — a runtime concatenation this
//! module already declines to follow (see the "complete literal" rule below). Resolved by hand,
//! `self.name` is the *module's* name, so `setUI('foo')` names `modules/<moduleName>/foo.otui` (the
//! module name equals its directory name in every real corpus case). Crucially, `setUI`'s effective
//! path is **module-root-relative** (`/<moduleName>/name`), *not* controller-relative like a bare
//! `loadUI('name')` — the two only
//! coincide for a top-level controller. So `setUI` is kept as its own [`UiLoadKind::SetUi`] and the
//! downstream resolver ([`crate::otmod`] consumers / the server's module scan) resolves it against
//! the module root, so a nested controller's `setUI('foo')` still names `<moduleRoot>/foo.otui`
//! rather than `<controllerSubdir>/foo.otui`. This crate stays free of `self.name`/manifest
//! knowledge (see [`crate::otmod::otmod_scripts`] for where module identity lives) — it only tags
//! the call kind; the resolver applies the module-root rule. The call is a *method* call
//! (`controllerVar:setUI(...)`, a colon), so the scan matches the bare word `setUI` **only when the
//! byte before it is a `:`** (`require_colon_prefix`) — a bare `setUI(`, a field call `foo.setUI(`,
//! and `g_ui.setUI(` (no such engine function) are all rejected, as is a longer identifier
//! (`mySetUI`/`setUIState`) by the whole-word boundary. The one other `setUI`-shaped text in the
//! engine, the method's own definition (`function Controller:setUI(name, parent)`), is naturally
//! excluded: its first "argument" is the bare identifier `name`, not a string literal, so the same
//! complete-literal-first-argument rule that already rejects a variable argument rejects it too — no
//! special-casing needed.
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

/// Which of the four UI-load call forms a [`UiLoadRef`] was found as.
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
    /// `controllerVar:setUI('name'[, parent])` — `Controller:setUI`, which records the name for a
    /// later `Controller:loadUI('/' .. self.name .. '/' .. name)` call; see the module doc comment's
    /// "a fourth, indirect form" section for the engine trace. Unlike [`Self::LoadUi`], its name is
    /// resolved **against the module root** (`/<moduleName>/name`), not the calling controller's own
    /// directory — the resolver must keep this kind distinct to apply that rule.
    SetUi,
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

/// Find every `g_ui.loadUI`/`g_ui.displayUI`/`g_ui.importStyle`/`setUI` call in `source` whose
/// first argument is a complete string literal (module doc comment). Comments and unrelated string
/// literals are excluded first, exactly like [`crate::lua_refs::scan_id_refs`]. The returned refs
/// are ordered by their span's start offset.
#[must_use]
pub fn scan_ui_loads(source: &str) -> Vec<UiLoadRef> {
    let excluded = excluded_ranges(source);
    let mut out: Vec<UiLoadRef> =
        call_first_string_literal(source, &excluded, "g_ui.loadUI", false)
            .map(|(path, span)| UiLoadRef {
                path,
                span,
                kind: UiLoadKind::LoadUi,
            })
            .chain(
                call_first_string_literal(source, &excluded, "g_ui.displayUI", false).map(
                    |(path, span)| UiLoadRef {
                        path,
                        span,
                        kind: UiLoadKind::DisplayUi,
                    },
                ),
            )
            .chain(
                call_first_string_literal(source, &excluded, "g_ui.importStyle", false).map(
                    |(path, span)| UiLoadRef {
                        path,
                        span,
                        kind: UiLoadKind::ImportStyle,
                    },
                ),
            )
            .chain(
                // `setUI` is a METHOD call (`controllerVar:setUI(...)`), matched as a bare word but
                // REQUIRING a `:` immediately before it (`require_colon_prefix`), so a bare `setUI(`,
                // a field call `foo.setUI(`, or `g_ui.setUI(` never match. The method's own definition
                // (`function Controller:setUI(name, parent)`) is excluded because its first "argument"
                // `name` is a bare identifier, not a string literal — see the module doc comment.
                call_first_string_literal(source, &excluded, "setUI", true).map(|(path, span)| {
                    UiLoadRef {
                        path,
                        span,
                        kind: UiLoadKind::SetUi,
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
    require_colon_prefix: bool,
) -> impl Iterator<Item = (String, ByteSpan)> + 'a {
    source.match_indices(name).filter_map(move |(idx, _)| {
        if !is_ident_boundary_before(source, idx)
            || !is_ident_boundary_after(source, idx + name.len())
        {
            return None;
        }
        // `setUI` is only ever the Lua method call `receiver:setUI(...)` — the byte immediately
        // before it must be a `:`. This rejects a bare `setUI(...)`, a field call `foo.setUI(...)`,
        // and `g_ui.setUI(...)` (there is no such engine function), any of which would otherwise
        // whole-word-match and fabricate a controller/UI pairing.
        if require_colon_prefix && idx.checked_sub(1).map(|i| source.as_bytes()[i]) != Some(b':') {
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
    fn set_ui_with_a_bare_name_is_indexed() {
        // Real corpus shape (`game_highscore.lua` and friends): a colon-qualified `ctrl:setUI('foo')`
        // method call. The scanned path is the bare name; how it resolves (module-root, not
        // controller-relative) is the downstream resolver's job — see the module doc comment.
        let src = "function init()\n  controller:setUI('game_highscore')\nend\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, UiLoadKind::SetUi);
        assert_eq!(refs[0].path, "game_highscore");
        assert_eq!(text(src, refs[0].span), "game_highscore");
    }

    #[test]
    fn set_ui_without_a_colon_receiver_is_not_indexed() {
        // Only the Controller method call `receiver:setUI(...)` is a real pairing. A bare `setUI(`,
        // a field call `foo.setUI(`, and `g_ui.setUI(` (no such engine function) must all be
        // rejected — the byte before `setUI` must be a `:`.
        assert!(scan_ui_loads("setUI('x')\n").is_empty());
        assert!(scan_ui_loads("foo.setUI('x')\n").is_empty());
        assert!(scan_ui_loads("g_ui.setUI('x')\n").is_empty());
    }

    #[test]
    fn set_ui_with_a_parent_second_argument_is_indexed() {
        let src = "controller:setUI('options', parentWidget)\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, UiLoadKind::SetUi);
        assert_eq!(refs[0].path, "options");
    }

    #[test]
    fn set_ui_method_definition_site_is_not_indexed() {
        // `Controller:setUI`'s own definition (`modules/modulelib/controller.lua`): the first
        // "argument" is the bare identifier `name`, not a string literal, so it is rejected by the
        // same rule that rejects any variable argument — no special-casing needed.
        let src = "function Controller:setUI(name, parent)\n  self.dataUI = { name = name, parent = parent }\nend\n";
        assert!(scan_ui_loads(src).is_empty());
    }

    #[test]
    fn set_ui_inside_a_comment_is_not_indexed() {
        let src = "-- controller:setUI('ghost')\n";
        assert!(scan_ui_loads(src).is_empty());
    }

    #[test]
    fn set_ui_as_a_substring_of_a_longer_identifier_is_not_indexed() {
        // The bare-word scan must reject identifiers that merely CONTAIN `setUI`: the whole-word
        // boundary check guards both sides — a preceding ident byte (`resetUI`) and a following one
        // (`setUIState`) both disqualify the match.
        assert!(scan_ui_loads("controller:resetUI('x')\n").is_empty());
        assert!(scan_ui_loads("controller:setUIState('x')\n").is_empty());
    }

    #[test]
    fn set_ui_inside_a_string_is_not_indexed() {
        let src = "local code = [[ controller:setUI('ghost') ]]\ncontroller:setUI('real')\n";
        let refs = scan_ui_loads(src);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "real");
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
    fn all_four_forms_in_one_file_are_all_found_in_span_order() {
        let src = "g_ui.importStyle('style.otui')\ng_ui.loadUI('a')\ng_ui.displayUI('b')\n\
                   controller:setUI('c')\n";
        let refs = scan_ui_loads(src);
        let kinds: Vec<UiLoadKind> = refs.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                UiLoadKind::ImportStyle,
                UiLoadKind::LoadUi,
                UiLoadKind::DisplayUi,
                UiLoadKind::SetUi,
            ]
        );
        let paths: Vec<&str> = refs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["style.otui", "a", "b", "c"]);
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
