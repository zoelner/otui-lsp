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
use crate::style_index::{is_native_base, StyleDef, StyleIndex};
use std::collections::HashSet;

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
}

impl WidgetAncestry {
    /// Whether any widget in the ancestry declares `prop` as a Lua custom style property (per
    /// `lua`). This is the membership test [`diagnostics`](crate::diagnostics) consults before
    /// emitting an `unknown-property` hint.
    #[must_use]
    pub fn declares_custom_property(&self, lua: &LuaWidgetIndex, prop: &str) -> bool {
        self.chain.iter().any(|name| lua.declares(name, prop))
    }

    /// Every Lua custom style property valid on this widget — the union of the `custom_props` of
    /// each widget in the ancestry (per `lua`), sorted and de-duplicated. This is the enumeration
    /// [`completion`](crate::completion) offers, the counterpart to the
    /// [`declares_custom_property`](Self::declares_custom_property) membership test.
    #[must_use]
    pub fn custom_properties(&self, lua: &LuaWidgetIndex) -> std::collections::BTreeSet<String> {
        let mut props = std::collections::BTreeSet::new();
        for name in &self.chain {
            for def in lua.lookup(name) {
                props.extend(def.custom_props.iter().cloned());
            }
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

    // Phase 1: the cross-file `.otui` style chain. `seen.insert` is false on a repeat, so a cycle
    // exits the loop without re-pushing.
    let mut current = start.to_owned();
    while seen.insert(current.clone()) {
        chain.push(current.clone());
        if is_native_base(&current) {
            native = Some(current.clone());
            break;
        }
        match pick_base(styles, &current) {
            Some(base) => current = base,
            None => break, // undefined base or malformed header — dead end, no native class
        }
    }

    // Phase 2: the native class's Lua parent chain (only if the `.otui` chain reached a native class).
    let Some(native_name) = native.clone() else {
        return WidgetAncestry { chain, native };
    };
    let mut current = native_name;
    while let Some(parent) = lua.parent_of(&current) {
        let parent = parent.to_owned();
        if !seen.insert(parent.clone()) {
            break; // cycle guard
        }
        chain.push(parent.clone());
        current = parent;
    }

    WidgetAncestry { chain, native }
}

/// The base of the deterministically-chosen definition of the style `name`, or `None` when `name` is
/// defined nowhere (or only by malformed headers with no base).
///
/// Duplicate style names are legal (the engine's last registration wins at runtime, which a static
/// index cannot know), so this picks a **stable** winner rather than guessing runtime order: among
/// the defs that carry a base, the one ordered first by `(document id, name-span start)`. The choice
/// is arbitrary but deterministic, so resolution never flickers between runs; a base that actually
/// differs across duplicates is pathological.
fn pick_base(styles: &StyleIndex, name: &str) -> Option<String> {
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
    defs.first().and_then(|(_, d)| d.base.clone())
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
        assert_eq!(a.chain, ["UIA", "UIB"]);
        assert_eq!(a.native.as_deref(), Some("UIA"));
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
