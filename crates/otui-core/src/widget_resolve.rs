//! Resolving a widget's full ancestry — the substrate for **widget-aware** property validation.
//!
//! A Lua-added style property (see [`lua_widgets`](crate::lua_widgets)) is valid only on a widget
//! whose type *is-a* the widget that declared it: `column-style` is valid on `UITable` and anything
//! descending from it, but not on a `Button`. Answering "is property `p` valid on a node of type
//! `T`?" therefore means walking `T`'s full inheritance, which spans **two** namespaces:
//!
//! 1. the `.otui` `Name < Base` chain ([`StyleIndex`]) — cross-file, e.g. `MyTable < Table < UITable`
//!    — walked until it reaches a **native** `UI*` class ([`is_native_base`]); then
//! 2. that native class's **Lua** parent chain ([`LuaWidgetIndex`]) — `UITable -> UIWidget` — since
//!    a custom property declared on a Lua ancestor is inherited too.
//!
//! [`resolve_ancestry`] produces the ordered list of every type name on that path; the diagnostics
//! pass then accepts a property that the global C++ catalog does not know **iff** some widget in the
//! ancestry declares it as a Lua custom property.
//!
//! Like the indexes it reads, this module is pure: it takes the two indexes by reference and returns
//! owned `String`s, with no I/O and no `lsp-types`.

use crate::lua_widgets::LuaWidgetIndex;
use crate::schema;
use crate::style_index::{StyleDef, StyleIndex, is_native_base};
use std::collections::HashSet;
use tree_sitter::Node;

/// The type name of the widget enclosing `start`: the `tag` of the nearest ancestor `container` (at
/// or above `start`) or the `base` of the nearest ancestor `style_header`. `None` when `start` has no
/// such ancestor. Shared by [`completion`](crate::completion) (widget-aware key completion) and
/// [`property_hover`](crate::property_hover) (per-widget property hover) — the same "what widget owns
/// this position" question, asked from two different starting points.
///
/// `line_skip`, when `Some(line_start)`, makes any node whose [`Node::start_byte`] is `>= line_start`
/// invisible to the match — completion's mid-edit guard. Completion runs against a possibly-broken
/// CST where the half-typed token on the cursor's own line frequently parses as a bare `container` tag
/// (a lowercase word with no `:` yet is grammatically a widget tag), which is **not** the enclosing
/// widget — it is the property being typed; passing the cursor's line start as `line_skip` skips it, so
/// only a widget declared on an earlier line counts as a genuine encloser. Hover starts from an
/// already-resolved `property_key` leaf in a document that parsed successfully, so it passes `None`
/// (no node is ever skipped) and simply walks up.
#[must_use]
pub fn enclosing_widget_type(
    start: Node,
    source: &str,
    line_skip: Option<usize>,
) -> Option<String> {
    let mut node = start;
    loop {
        let skip = line_skip.is_some_and(|line_start| node.start_byte() >= line_start);
        if !skip {
            match node.kind() {
                "container" => {
                    return node
                        .child_by_field_name("tag")
                        .map(|tag| source[tag.start_byte()..tag.end_byte()].to_owned());
                }
                "style_header" => {
                    return node
                        .child_by_field_name("base")
                        .map(|base| source[base.start_byte()..base.end_byte()].to_owned());
                }
                _ => {}
            }
        }
        node = node.parent()?;
    }
}

/// The resolved ancestry of a widget type: every type name from the starting type up to the root of
/// its inheritance, nearest-first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetAncestry {
    /// The ordered ancestry, nearest-first: the `.otui` style chain, the native `UI*` class it
    /// resolves to (if any), and that class's Lua parent chain. De-duplicated — a cycle stops the
    /// walk rather than repeating a name.
    pub chain: Vec<String>,
    /// The native `UI*` widget the `.otui` chain resolved to, or `None` when the chain dead-ended at
    /// an undefined base or a malformed header before reaching a native class.
    pub native: Option<String>,
    /// How many of `chain`'s leading entries belong to the `.otui` `Name < Base` walk itself
    /// (`chain[..otui_chain_len]`) — ending at the native class when [`native`](Self::native) is
    /// `Some`, or at the dead end / cycle when it is `None`. Every entry from `otui_chain_len`
    /// onward is a *different* namespace: a native class's Lua parent chain, or a `__class:`
    /// re-root's own Lua ancestry. A caller that wants only the declared style inheritance — not
    /// the Lua widget-class lineage used for property validation — must cut here, not merely stop
    /// at the first name it recognizes as native (see the [`hover`](crate::hover) module, which
    /// hit exactly this bug: an undefined-base chain plus a `__class:` re-root was leaking Lua
    /// class names into a display meant to show only `< Base` hops).
    pub otui_chain_len: usize,
}

impl WidgetAncestry {
    /// Whether any widget in the ancestry declares `prop` as a custom style property — either in
    /// **Lua** (per `lua`, e.g. `UITable` reading `column-style`) or in a **native C++**
    /// `onStyleApply` override (per [`schema::native_widget_declares`], e.g. `UITextEdit` reading
    /// `placeholder`). This is the membership test [`diagnostics`](crate::diagnostics) consults
    /// before emitting an `unknown-property` hint.
    ///
    /// Both sources are per-widget, so the property is accepted only when the widget *is-a* the
    /// declarer: `placeholder` resolves on `TextEdit < UITextEdit` but not on a `Button`, where the
    /// engine reads it nowhere.
    #[must_use]
    pub fn declares_custom_property(&self, lua: &LuaWidgetIndex, prop: &str) -> bool {
        self.chain
            .iter()
            .any(|name| lua.declares(name, prop) || schema::native_widget_declares(name, prop))
    }

    /// Every custom style property valid on this widget — the union, over the whole ancestry, of the
    /// Lua-declared `custom_props` (per `lua`) and the native C++ `onStyleApply` tags (per
    /// [`schema::native_widget_properties`]), sorted and de-duplicated. This is the enumeration
    /// [`completion`](crate::completion) offers, the counterpart to the
    /// [`declares_custom_property`](Self::declares_custom_property) membership test.
    #[must_use]
    pub fn custom_properties(&self, lua: &LuaWidgetIndex) -> std::collections::BTreeSet<String> {
        let mut props = std::collections::BTreeSet::new();
        for name in &self.chain {
            for def in lua.lookup(name) {
                props.extend(def.custom_props.iter().cloned());
            }
            props.extend(
                schema::native_widget_properties(name)
                    .iter()
                    .map(|p| (*p).to_owned()),
            );
        }
        props
    }
}

/// Resolve the full ancestry of the widget type `start`.
///
/// Walks two chains in sequence (see the module docs): the `.otui` `Name < Base` chain via `styles`
/// until a native `UI*` base or a dead end, then the Lua parent chain of that native class via
/// `lua`. A `HashSet` guard makes both walks cycle-safe — a base that loops back to an already-seen
/// name stops the walk — and a base that resolves to no definition (and is not native) also stops
/// it, leaving [`native`](WidgetAncestry::native) `None`.
#[must_use]
pub fn resolve_ancestry(start: &str, styles: &StyleIndex, lua: &LuaWidgetIndex) -> WidgetAncestry {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut native = None;
    // Widget classes named by a `__class:` on some style in the chain. Each re-roots the widget onto
    // a class its `< Base` chain never mentions, so each seeds its own Lua-parent walk in phase 2.
    let mut reroots: Vec<String> = Vec::new();

    // Phase 1: the cross-file `.otui` style chain. `seen.insert` is false on a repeat, so a cycle
    // exits the loop without re-pushing.
    let mut current = start.to_owned();
    while seen.insert(current.clone()) {
        chain.push(current.clone());
        // A **defined** style wins over the native-name heuristic. [`is_native_base`] only asks
        // whether a name *looks* like a built-in (`UI` + uppercase); it cannot know that the engine's
        // own `data/styles/10-items.otui` declares `UIDragIcon < UIItem` — a user style wearing a
        // native-looking name. Treating that as a built-in stops the walk dead, so `UIDragIcon` never
        // reaches `UIItem` and loses every property `UIItem` declares (`virtual`, `item-id`, …).
        //
        // So: walk a definition whenever one exists, and fall back to the heuristic only for a name
        // nothing defines — which is exactly what a genuine built-in is.
        let Some(def) = pick_def(styles, &current) else {
            if is_native_base(&current) {
                native = Some(current.clone());
            }
            break; // a built-in, or an undefined base / malformed header — either way, the walk ends
        };
        // `__class:` names the class the engine actually instantiates for this style, regardless of
        // what it inherits its look from. Record it; the base walk continues as normal.
        if let Some(class) = &def.lua_class {
            reroots.push(class.clone());
        }
        match &def.base {
            Some(base) => current = base.clone(),
            None => break,
        }
    }
    // The `.otui` walk ends here — everything pushed to `chain` above is a declared `< Base` hop
    // (or, when reached, the native class the header stops at). Recorded now, before phase 2 can
    // push anything else, so callers that want *only* this portion don't have to guess a cutoff
    // from `chain`'s contents (see `otui_chain_len`'s doc comment).
    let otui_chain_len = chain.len();

    // Phase 2: the Lua parent chain of each class we landed on — the native `UI*` the style chain
    // reached, plus every `__class:` re-root. A `__class` class (e.g. `UISpinBox`) is typically a Lua
    // widget, so its own `extends` parents carry properties too.
    // `seen` and the traversal guard are deliberately *separate* sets. `seen` de-duplicates entries
    // in `chain`; `expanded` stops the walk from re-visiting a class. Using `seen` for both would cut
    // the walk short at a class that is already in the chain but whose own Lua parents were never
    // traversed — e.g. a `__class:` re-root extending a name that also appears in the `.otui` chain
    // would silently lose every property inherited above it.
    let mut expanded: HashSet<String> = HashSet::new();
    for root in native.iter().cloned().chain(reroots) {
        let mut current = root;
        while expanded.insert(current.clone()) {
            if seen.insert(current.clone()) {
                chain.push(current.clone());
            }
            let Some(parent) = lua.parent_of(&current) else {
                break;
            };
            current = parent.to_owned();
        }
    }

    // Phase 3: `UIWidget` is the implicit root of every widget — every native `UI*` widget class
    // derives from it in the engine, whether or not a Lua `extends` line spells that out (the C++
    // ones never do). It matters because a module can attach style properties to `UIWidget` itself
    // via a `connect(UIWidget, {onStyleApply = …})` hook — that is how `tooltip:` becomes valid on
    // *any* widget — and those would otherwise resolve on nothing.
    //
    // Only when the chain actually reached a native class: a chain that dead-ended at an undefined
    // base tells us nothing about what it derives from, and assuming `UIWidget` there would start
    // accepting properties on a typo'd widget name.
    if native.is_some() && seen.insert(UI_WIDGET.to_owned()) {
        chain.push(UI_WIDGET.to_owned());
    }

    WidgetAncestry {
        chain,
        native,
        otui_chain_len,
    }
}

/// The engine's root widget class, the implicit ancestor of every widget.
const UI_WIDGET: &str = "UIWidget";

/// The deterministically-chosen definition of the style `name`, or `None` when `name` is defined
/// nowhere (or only by malformed headers with no base). Carries both the base to keep walking and
/// any `__class:` re-root the style declares.
///
/// Duplicate style names are legal (the engine's last registration wins at runtime, which a static
/// index cannot know), so this picks a **stable** winner rather than guessing runtime order: among
/// the defs that carry a base, the one ordered first by `(document id, name-span start)`. The choice
/// is arbitrary but deterministic, so resolution never flickers between runs; a base that actually
/// differs across duplicates is pathological.
fn pick_def<'a>(styles: &'a StyleIndex, name: &str) -> Option<&'a StyleDef> {
    let mut defs: Vec<(&str, &StyleDef)> = styles
        .lookup(name)
        .into_iter()
        .filter(|(_, d)| d.base.is_some())
        .map(|(doc, d)| (doc.as_str(), d))
        .collect();
    defs.sort_by(|a, b| {
        a.0.cmp(b.0)
            .then(a.1.name_span.start.cmp(&b.1.name_span.start))
    });
    defs.first().map(|(_, d)| *d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lua_widgets::scan_widgets;
    use crate::style_index::extract_style_defs;
    use crate::syntax::SyntaxTree;

    /// Build a [`StyleIndex`] from `(doc, otui_source)` pairs.
    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(*doc, extract_style_defs(&tree));
        }
        index
    }

    /// Build a [`LuaWidgetIndex`] from `(doc, lua_source)` pairs.
    fn lua(docs: &[(&str, &str)]) -> LuaWidgetIndex {
        let mut index = LuaWidgetIndex::new();
        for (doc, src) in docs {
            index.set_document(*doc, scan_widgets(src));
        }
        index
    }

    #[test]
    fn resolves_cross_file_otui_chain_then_lua_parents() {
        // MyTable < Table (a.otui) and Table < UITable (b.otui): the .otui chain crosses files and
        // ends at the native UITable, whose Lua parent chain is UITable -> UIWidget.
        let styles = styles(&[
            ("a.otui", "MyTable < Table\n"),
            ("b.otui", "Table < UITable\n"),
        ]);
        let lua = lua(&[("uitable.lua", "UITable = extends(UIWidget, 'UITable')\n")]);

        let a = resolve_ancestry("MyTable", &styles, &lua);
        assert_eq!(a.chain, ["MyTable", "Table", "UITable", "UIWidget"]);
        assert_eq!(a.native.as_deref(), Some("UITable"));
    }

    #[test]
    fn starting_at_a_native_name_walks_only_the_lua_chain() {
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", "UITable = extends(UIWidget, 'UITable')\n")]);

        let a = resolve_ancestry("UITable", &styles, &lua);
        assert_eq!(a.chain, ["UITable", "UIWidget"]);
        assert_eq!(a.native.as_deref(), Some("UITable"));
    }

    #[test]
    fn cycle_in_the_otui_chain_stops_without_looping() {
        // A < B and B < A: the walk must terminate (no native class reached).
        let styles = styles(&[("a.otui", "A < B\n"), ("b.otui", "B < A\n")]);
        let lua = lua(&[]);

        let a = resolve_ancestry("A", &styles, &lua);
        assert_eq!(a.chain, ["A", "B"]);
        assert_eq!(a.native, None);
    }

    #[test]
    fn missing_base_stops_without_a_native_class() {
        // `Nonexistent` is neither native nor defined anywhere: the walk dead-ends there.
        let styles = styles(&[("a.otui", "X < Nonexistent\n")]);
        let lua = lua(&[]);

        let a = resolve_ancestry("X", &styles, &lua);
        assert_eq!(a.chain, ["X", "Nonexistent"]);
        assert_eq!(a.native, None);
    }

    #[test]
    fn a_bare_undefined_start_has_no_native_class() {
        // A widget instance whose type is defined nowhere resolves to just itself.
        let a = resolve_ancestry("Button", &styles(&[]), &lua(&[]));
        assert_eq!(a.chain, ["Button"]);
        assert_eq!(a.native, None);
    }

    #[test]
    fn duplicate_style_name_resolves_deterministically() {
        // `Dup` is declared with two different bases in two files. The pick is stable across runs
        // (ordered by document id), so the resolved native is always the same one.
        let styles = styles(&[
            ("a.otui", "Dup < UIWidget\n"),
            ("b.otui", "Dup < UIButton\n"),
        ]);
        let lua = lua(&[]);

        let first = resolve_ancestry("Dup", &styles, &lua);
        // "a.otui" sorts before "b.otui", so its base (UIWidget) wins.
        assert_eq!(first.native.as_deref(), Some("UIWidget"));
        // Stable: repeated resolution yields the same answer.
        for _ in 0..8 {
            assert_eq!(resolve_ancestry("Dup", &styles, &lua), first);
        }
    }

    #[test]
    fn lua_parent_cycle_is_guarded() {
        // Pathological mutual Lua inheritance UIA <-> UIB must not loop.
        let styles = styles(&[]);
        let lua = lua(&[(
            "w.lua",
            "UIA = extends(UIB, 'UIA')\nUIB = extends(UIA, 'UIB')\n",
        )]);

        let a = resolve_ancestry("UIA", &styles, &lua);
        // UIA is native, so phase 2 follows UIA -> UIB, then UIB -> UIA is already seen and stops.
        // `UIWidget` is then appended as the implicit root of every widget (phase 3).
        assert_eq!(a.chain, ["UIA", "UIB", "UIWidget"]);
        assert_eq!(a.native.as_deref(), Some("UIA"));
    }

    #[test]
    fn ui_widget_is_the_implicit_root_of_a_resolved_widget() {
        // Every native widget derives from `UIWidget` in the engine, even though the C++ classes have
        // no Lua `extends` line saying so. A module hooking style properties onto `UIWidget` (see
        // `connect(UIWidget, {onStyleApply = …})`) must therefore reach every widget.
        let styles = styles(&[("a.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);

        let button = resolve_ancestry("Button", &styles, &lua);
        assert_eq!(button.chain, ["Button", "UIButton", "UIWidget"]);
    }

    #[test]
    fn a_dead_end_chain_does_not_get_the_implicit_root() {
        // A chain that never reached a native class tells us nothing about what it derives from —
        // assuming `UIWidget` there would start accepting properties on a typo'd widget name.
        let styles = styles(&[("a.otui", "Thing < NoSuchBase\n")]);
        let lua = lua(&[]);

        let thing = resolve_ancestry("Thing", &styles, &lua);
        assert!(!thing.chain.contains(&"UIWidget".to_owned()));
        assert_eq!(thing.native, None);
    }

    #[test]
    fn a_user_style_with_a_native_looking_name_is_still_walked() {
        // The engine's own `data/styles/10-items.otui` declares `UIDragIcon < UIItem` — a *user*
        // style whose name trips the `UI`-prefix heuristic. Treating it as a built-in stops the walk
        // at `UIDragIcon`, so it never reaches `UIItem` and loses `virtual` / `item-id` / ….
        let styles = styles(&[("a.otui", "UIDragIcon < UIItem\n")]);
        let lua = lua(&[]);

        let icon = resolve_ancestry("UIDragIcon", &styles, &lua);
        assert_eq!(icon.chain, ["UIDragIcon", "UIItem", "UIWidget"]);
        assert_eq!(icon.native.as_deref(), Some("UIItem"));
        // `virtual` is one of `UIItem`'s native C++ `onStyleApply` tags.
        assert!(icon.declares_custom_property(&lua, "virtual"));
    }

    #[test]
    fn an_undefined_native_looking_name_is_still_treated_as_a_built_in() {
        // Nothing defines `UIButton`, so the heuristic still applies — that is what a genuine
        // built-in looks like, and the walk must land on it as the native class.
        let styles = styles(&[("a.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);

        let button = resolve_ancestry("Button", &styles, &lua);
        assert_eq!(button.native.as_deref(), Some("UIButton"));
        assert_eq!(button.chain, ["Button", "UIButton", "UIWidget"]);
    }

    #[test]
    fn a_class_reroot_pulls_in_the_lua_widgets_properties() {
        // `SpinBox < TextEdit` + `__class: UISpinBox` — the engine instantiates a `UISpinBox` (which
        // declares `minimum`/`maximum`/`step` in Lua) styled from `TextEdit`. The `< Base` chain
        // alone never mentions `UISpinBox`, so without the re-root those properties look unknown.
        let styles = styles(&[(
            "a.otui",
            "TextEdit < UITextEdit\nSpinBox < TextEdit\n  __class: UISpinBox\n",
        )]);
        let lua = lua(&[(
            "uispinbox.lua",
            "UISpinBox = extends(UITextEdit, 'UISpinBox')\n\
             function UISpinBox:onStyleApply(styleName, styleNode)\n\
               for name, value in pairs(styleNode) do\n\
                 if name == 'minimum' then self:setMinimum(value)\n\
                 elseif name == 'maximum' then self:setMaximum(value)\n\
                 end\n\
               end\n\
             end\n",
        )]);

        let spin = resolve_ancestry("SpinBox", &styles, &lua);
        assert!(
            spin.chain.contains(&"UISpinBox".to_owned()),
            "{:?}",
            spin.chain
        );
        assert!(spin.declares_custom_property(&lua, "minimum"));
        assert!(spin.declares_custom_property(&lua, "maximum"));
        // The style chain is still walked, so the base's native properties still resolve.
        assert!(spin.declares_custom_property(&lua, "placeholder"));
    }

    #[test]
    fn a_reroots_lua_ancestors_survive_a_name_shared_with_the_otui_chain() {
        // The walk must not stop at a class merely because it is already in `chain`. Here the
        // re-rooted `UIBar` extends `Base` — a name the `.otui` chain already carries — and `Base`
        // in turn has a Lua parent (`UICore`) that declares a property. Guarding the traversal with
        // the chain's de-dup set would break at `Base` and silently drop `core-prop`.
        let styles = styles(&[("a.otui", "Base < UIFoo\nThing < Base\n  __class: UIBar\n")]);
        let lua = lua(&[(
            "w.lua",
            "UIBar = extends(Base, 'UIBar')\n\
             Base = extends(UICore, 'Base')\n\
             function UICore:onStyleApply(styleName, styleNode)\n\
               for name, value in pairs(styleNode) do\n\
                 if name == 'core-prop' then self:setCore(value) end\n\
               end\n\
             end\n",
        )]);

        let thing = resolve_ancestry("Thing", &styles, &lua);
        assert!(
            thing.chain.contains(&"UICore".to_owned()),
            "the re-root's grandparent was dropped: {:?}",
            thing.chain
        );
        assert!(thing.declares_custom_property(&lua, "core-prop"));
    }

    #[test]
    fn a_class_reroot_does_not_leak_to_the_base_style() {
        // The re-root belongs to `SpinBox`, not to the `TextEdit` it inherits its look from.
        let styles = styles(&[(
            "a.otui",
            "TextEdit < UITextEdit\nSpinBox < TextEdit\n  __class: UISpinBox\n",
        )]);
        let lua = lua(&[(
            "uispinbox.lua",
            "UISpinBox = extends(UITextEdit, 'UISpinBox')\n\
             function UISpinBox:onStyleApply(s, n)\n\
               for name, value in pairs(n) do\n\
                 if name == 'minimum' then self:setMinimum(value) end\n\
               end\n\
             end\n",
        )]);

        assert!(
            !resolve_ancestry("TextEdit", &styles, &lua).declares_custom_property(&lua, "minimum")
        );
    }

    #[test]
    fn declares_a_native_cpp_widget_property() {
        // `UITextEdit::onStyleApply` (C++) dispatches `placeholder` — there is no Lua `extends` line
        // for it, so only the native table can accept it. It must resolve down the `.otui` chain.
        let styles = styles(&[("a.otui", "TextEdit < UITextEdit\nSearchBox < TextEdit\n")]);
        let lua = LuaWidgetIndex::new();

        let search = resolve_ancestry("SearchBox", &styles, &lua);
        assert!(search.declares_custom_property(&lua, "placeholder"));
        assert!(search.declares_custom_property(&lua, "max-length"));
        assert!(search.custom_properties(&lua).contains("multiline"));
    }

    #[test]
    fn a_native_widget_property_does_not_leak_to_unrelated_widgets() {
        // `change-cursor-image` is `UITextEdit`'s. On a Button the engine reads it nowhere, so it
        // must stay unknown — the per-widget table must not become a global one.
        let styles = styles(&[("a.otui", "Button < UIButton\nTextEdit < UITextEdit\n")]);
        let lua = LuaWidgetIndex::new();

        assert!(
            resolve_ancestry("TextEdit", &styles, &lua)
                .declares_custom_property(&lua, "change-cursor-image")
        );
        assert!(
            !resolve_ancestry("Button", &styles, &lua)
                .declares_custom_property(&lua, "change-cursor-image")
        );
    }

    #[test]
    fn declares_custom_property_via_a_native_ancestor() {
        // `column-style` is declared by UITable's Lua onStyleApply; it is valid on MyTable (which
        // descends from UITable) but not an arbitrary property, and never on an unrelated widget.
        let styles = styles(&[
            ("a.otui", "MyTable < Table\n"),
            ("b.otui", "Table < UITable\n"),
        ]);
        let lua = lua(&[(
            "uitable.lua",
            "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    end
  end
end
",
        )]);

        let table = resolve_ancestry("MyTable", &styles, &lua);
        assert!(table.declares_custom_property(&lua, "column-style"));
        assert!(!table.declares_custom_property(&lua, "not-a-prop"));

        // The same property is unknown on an unrelated widget (no UITable in its ancestry).
        let button = resolve_ancestry("UIButton", &styles, &lua);
        assert!(!button.declares_custom_property(&lua, "column-style"));
    }

    #[test]
    fn custom_properties_enumerates_the_whole_ancestry() {
        // A widget with props plus a Lua parent that also declares props: the enumeration is the
        // union across the chain (the substrate for widget-aware completion).
        let styles = styles(&[("a.otui", "MyTable < UITable\n")]);
        let lua = lua(&[(
            "uitable.lua",
            "\
UIScrollArea = extends(UIWidget, 'UIScrollArea')

function UIScrollArea:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'inverted-scroll' then
    end
  end
end

UITable = extends(UIScrollArea, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    elseif name == 'row-style' then
    end
  end
end
",
        )]);

        let props = resolve_ancestry("MyTable", &styles, &lua).custom_properties(&lua);
        let got: Vec<&str> = props.iter().map(String::as_str).collect();
        // UITable's own props plus the inherited one from its Lua parent UIScrollArea.
        assert_eq!(got, ["column-style", "inverted-scroll", "row-style"]);
    }
}
