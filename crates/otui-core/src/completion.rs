//! Closed-set completion (spec §6): what fixed vocabulary applies at a byte offset.
//!
//! This module answers "the cursor is here — which fixed OTML vocabulary should the client offer?"
//! It covers the four closed sets the [`schema`](crate::schema) module owns:
//!
//! * `$state` selector names ([`schema::STATES`]),
//! * `anchors.<edge>` edges ([`schema::ANCHOR_EDGES`] + [`schema::SHORTHAND_ANCHORS`]),
//! * anchor **target** keywords ([`schema::MAGIC_ANCHOR_TARGETS`]) plus the in-scope widget `id:`
//!   values reachable in the current document, and target-side edges, and
//! * `@event` handler names ([`schema::EVENTS`]),
//!
//! plus the **indented statement slot** — a position that is grammatically ambiguous between a
//! property key and a nested child widget, so (following consolidated LSPs) it offers the *union*,
//! ranked and tagged for the client to filter:
//!
//! * property **key** names — the global [`catalog::PROPERTIES`] plus the enclosing widget's
//!   Lua-added properties (resolved cross-file via [`widget_resolve`]), and
//! * child **widget** names — workspace `.otui` styles, Lua widget classes, and native `UI*` bases.
//!
//! plus **property values** for the fixed-value-set properties:
//!
//! * after `display:` / `layout:` — the keyword set ([`schema::DISPLAY_VALUES`] /
//!   [`schema::LAYOUT_TYPES`]), and after a color property (`color:`, `background-color:`, …) — the
//!   named-color list ([`catalog::NAMED_COLORS`]). The property's value kind comes from the audited
//!   [`property_hover::classify_value`].
//!
//! plus the **`layout:` block** — a separate, closed key namespace ([`schema::is_layout_block_property`]
//! / [`catalog::LAYOUT_PROPERTIES`]) valid only on the indented lines nested under a `layout:` header,
//! disjoint from both the ordinary property catalog and the `layout: <type>` shorthand value: a key
//! slot inside the block offers the block's own keys ([`Context::LayoutBlock`]), and a value slot
//! offers that key's engine-verified value set, if it has one ([`Context::LayoutValue`]).
//!
//! **Deliberately out of scope** (returns an empty vec): **freeform** property values — a number, an
//! asset path, the `border` shorthand, or an arbitrary string offer nothing (there is no closed set
//! to complete). When no set applies, this returns nothing — never a guess.
//!
//! No I/O and no `lsp-types`: byte offsets in, [`CompletionItem`]s out. The workspace-aware slot reads
//! the in-memory style + Lua indexes ([`complete_at_with_widgets`]); [`complete_at`] passes empty ones.
//!
//! ## How the context is classified
//!
//! Completion runs mid-edit, when the CST around the cursor is usually a half-typed `ERROR` region,
//! so a tree-shape match is unreliable here. Instead we classify from the **line prefix** — the text
//! from the line start up to the cursor — which is stable no matter how broken the rest of the line
//! is. The prefix rules mirror the grammar shapes (`crates/tree-sitter-otui/grammar.js`):
//!
//! * `state_selector` = `'$' repeat1(state) ':'` → a line whose first non-space char is `$` and
//!   which has no `:` yet is inside the selector → [`STATES`](crate::schema::STATES).
//! * `anchor_property` = `'anchors' '.' edge ':' anchor_target` → a line beginning `anchors.`:
//!   before the `:` we are on the **edge** key; after it, on the **target** value.
//! * `event_property` = `'@' event_name ':'` → a line whose first non-space char is `@` with no `:`
//!   yet is on the event **key** → [`EVENTS`](crate::schema::EVENTS). (After the `:` the value is
//!   embedded Lua — out of scope.)
//!
//! The one context the prefix alone cannot see is a multi-line `|` block-scalar body (raw Lua/text),
//! where a line may legitimately start with `$` / `@`. That is the only place we consult the CST: if
//! the cursor sits inside a `block_scalar_content` (or a `comment`), we suppress completion. The
//! grammar node kinds used are therefore `block_scalar_content` and `comment`.
//!
//! Everything here is pure: byte offsets in, [`CompletionItem`]s out. No I/O, no `lsp-types`.

use crate::catalog;
use crate::diagnostics;
use crate::indent;
use crate::lua_widgets::LuaWidgetIndex;
use crate::property_hover;
use crate::schema;
use crate::style_index::StyleIndex;
use crate::syntax::SyntaxTree;
use crate::widget_resolve;
use lang_api::{CompletionItem, CompletionKind, InsertFormat};
use tree_sitter::Node;

/// The closed set that applies at the cursor, once the line prefix has been classified.
enum Context {
    /// Inside a `$state` selector: offer the 14 state names.
    States,
    /// On the property-side `anchors.<edge>` key: offer the 6 edges plus the 2 shorthands.
    AnchorEdgeKey,
    /// On the target-side edge after `<targetId>.` in an anchor value: offer the 6 edges.
    AnchorTargetEdge,
    /// At the start of an anchor value (target position): offer the magic target keywords.
    AnchorTarget,
    /// On an `@event` key: offer the known event handler names.
    Events,
    /// On an indented statement slot being typed (no `:` yet): the position is grammatically
    /// ambiguous between a **property key** and a nested **child widget**, so offer the union —
    /// catalog + the widget's Lua properties + child-widget names — ranked, and let the client filter.
    IndentedSlot,
    /// In the **value** position of an ordinary `key: value` property whose value is a fixed set:
    /// offer that set (`display`/`layout` keywords, or named colors for a color property). The string
    /// is the property key. A freeform-valued property yields nothing.
    PropertyValue(String),
    /// On an indented **key** slot nested inside a `layout:` block (see [`enclosing_layout_block`]):
    /// offer the block's own closed key set ([`catalog::LAYOUT_PROPERTIES`]) — never the global
    /// catalog or child-widget names, since a `layout:` block holds only layout properties.
    LayoutBlock,
    /// In the **value** position of a `layout:`-block key (`type`, `fit-children`, …): offer that
    /// key's engine-verified value set (see [`layout_value_items`]), or nothing for a free
    /// numeric/dimension key. The string is the layout-block key.
    LayoutValue(String),
    /// In the **base slot** of a top-level `Name < Base` style header (spec §6, §2.2): past the
    /// `<`, offer the valid-base union — every workspace style name plus the `UI*` native classes
    /// in use ([`widget_names`]). A base is a plain type reference (not a child statement), so no
    /// `id:` snippet follows it, unlike [`Context::IndentedSlot`]'s child-widget entries.
    BaseSlot,
}

/// Compute the completion candidates for the cursor at byte `offset` in `source` (spec §6).
///
/// Returns the matching set — `$state` names, anchor edges/targets, `@event` names, or (on an
/// indented bare `key`) the catalog property names — or an empty vec when the cursor is not in one of
/// those contexts (a property **value**, a Lua body, an out-of-range offset, …). The client is
/// expected to filter the returned set by the partial word it already has; we always return the whole
/// set so the labels are deterministic (schema/catalog const order).
#[must_use]
pub fn complete_at(source: &str, offset: usize) -> Vec<CompletionItem> {
    // The pure, workspace-unaware form: no style/Lua indexes, so a property key offers only the
    // global C++ catalog (the widget-added Lua properties come from the workspace-aware variant).
    complete_at_with_widgets(source, offset, &StyleIndex::new(), &LuaWidgetIndex::new())
}

/// Like [`complete_at`], but **widget-aware**: on an ordinary property key it additionally offers the
/// custom style properties the enclosing widget adds in Lua (e.g. `column-style` under a `UITable`),
/// resolved from the workspace `styles` + `lua` indexes. Every other context is identical. With empty
/// indexes it degrades exactly to [`complete_at`].
#[must_use]
pub fn complete_at_with_widgets(
    source: &str,
    offset: usize,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
) -> Vec<CompletionItem> {
    // Guard against an offset past the end or inside a multi-byte char: slicing the prefix below
    // would otherwise panic, and an offscreen cursor has no context anyway.
    if offset > source.len() || !source.is_char_boundary(offset) {
        return Vec::new();
    }

    // Parse once and share the tree with the CST-consulting steps below (the block-scalar/comment
    // suppression check and, for the indented slot, the enclosing-widget lookup) — completion runs
    // per keystroke, so a single parse per request matters. A parse failure degrades gracefully:
    // nothing is suppressed and no enclosing widget is found.
    let tree = SyntaxTree::parse(source);

    // A `|` block-scalar body is raw Lua/text: a line inside it may start with `$`/`@` yet is not
    // OTML markup, so suppress there. This is the only place the CST is consulted (see module docs).
    if tree
        .as_ref()
        .is_some_and(|t| in_suppressed_context(t, offset))
    {
        return Vec::new();
    }

    let line_start = source[..offset].rfind('\n').map_or(0, |nl| nl + 1);
    let prefix = &source[line_start..offset];

    match classify(prefix) {
        // A property key additionally requires structurally valid indentation: an odd indent or a
        // depth jump of more than one level is a hard OTML parse error (the `diagnostics` module
        // flags `odd-indentation` / `invalid-indentation-depth`), so no key may be offered there.
        // This needs the whole document (the preceding lines' depth), so it is gated here rather
        // than in the line-prefix classifier. The closed-set contexts keep their own guards.
        Some(Context::IndentedSlot) if !diagnostics::line_indentation_is_valid(source, offset) => {
            Vec::new()
        }
        // An empty slot sitting *before* an existing special-form token ($state / @event / anchors. /
        // &alias / !expr / `-` list) is not a property/widget position: the line's real content is a
        // special form the (empty) prefix cannot see. Offer nothing rather than the wrong union.
        Some(Context::IndentedSlot)
            if prefix.trim().is_empty() && rest_of_line_is_special_form(source, offset) =>
        {
            Vec::new()
        }
        // A key slot nested under a `layout:` header is a separate, closed key namespace — reroute
        // before falling into the ordinary catalog+widget-names union (see `enclosing_layout_block`).
        Some(Context::IndentedSlot) if enclosing_layout_block(source, offset) => {
            items_for(Context::LayoutBlock, source, offset)
        }
        Some(Context::IndentedSlot) => {
            indented_slot_items(tree.as_ref(), source, offset, styles, lua)
        }
        // A property value on a structurally-invalid line (tab/odd/depth-jump) is not offered, same
        // gate as the key slot.
        Some(Context::PropertyValue(_))
            if !diagnostics::line_indentation_is_valid(source, offset) =>
        {
            Vec::new()
        }
        // A value slot for a genuine layout-block key (gated on both the key belonging to that
        // namespace AND the cursor actually sitting inside a `layout:` block) reroutes to the
        // block's own value sets rather than the global `classify_value` — the two namespaces are
        // disjoint by design (spec: layout keys are dispatched by the layout object, not the widget
        // style parser).
        Some(Context::PropertyValue(key))
            if schema::is_layout_block_property(&key) && enclosing_layout_block(source, offset) =>
        {
            items_for(Context::LayoutValue(key), source, offset)
        }
        // The style-header base slot is workspace-aware (the same union `widget_names` builds for
        // the child-widget slot), so it is built by `base_slot_items` and routed before
        // `items_for` is reached — mirrors `Context::IndentedSlot` above.
        Some(Context::BaseSlot) => base_slot_items(styles, lua),
        Some(context) => items_for(context, source, offset),
        None => Vec::new(),
    }
}

/// Whether the current line, from `offset` to its end, begins (after whitespace) with a non-property
/// special form — a `$state` selector, `@event`, `anchors.` object, `&alias`, `!expr`, or `-` list
/// item. Used to keep an empty indented slot from offering the property/widget union when the line's
/// real content (which the empty line prefix cannot see) is one of those.
fn rest_of_line_is_special_form(source: &str, offset: usize) -> bool {
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |nl| offset + nl);
    let rest = source[offset..line_end].trim_start();
    rest.starts_with(['$', '@', '&', '!', '-']) || rest.starts_with("anchors.")
}

/// Whether `offset` sits on a line **inside** a `layout:` block: the nearest preceding non-blank,
/// non-comment line is indented strictly less than the current line, AND its key (the text before its
/// first `:`, or the whole trimmed line if it has none) is exactly `layout`.
///
/// Both `layout:`-block forms count as that reference line: the bare block header (`layout:`, nothing
/// after the colon) and the header carrying its own shorthand value (`layout: verticalBox`) — the
/// engine applies a `layout:` node's children even when it already resolved a type from the leaf
/// value (`src/framework/ui/uiwidgetbasestyle.cpp`: `layoutType` is read from either the leaf value or
/// a nested `type:`, then `if (node->hasChildren()) m_layout->applyStyle(node)` runs unconditionally
/// right after). A shallower reference line with any other key is not a layout block, so an ordinary
/// nested property (`$hover:`'s children, a plain widget's properties, …) is unaffected.
///
/// Purely line/indentation-driven — the crate's shared [`indent`] primitives, never the CST — so it
/// keeps working while a sibling line in the block is still mid-edit. Deliberately does not special-
/// case a block-scalar body: no layout-block key's value is ever one, so that skip (which
/// [`crate::indent::indent_for_line`] performs for the on-type-indent use case) is not needed here.
pub(crate) fn enclosing_layout_block(source: &str, offset: usize) -> bool {
    let lines = indent::split_lines(source);
    let Some(idx) = lines
        .iter()
        .position(|l| offset >= l.start && offset <= l.start + l.text.len())
    else {
        return false;
    };
    let current_indent = indent::leading_spaces(lines[idx].text);
    if current_indent == 0 {
        return false;
    }
    for line in lines[..idx].iter().rev() {
        let trimmed = line.text.trim();
        if trimmed.is_empty() || indent::is_comment(trimmed) {
            continue;
        }
        let sp = indent::leading_spaces(line.text);
        if sp < current_indent {
            let key = trimmed.split(':').next().unwrap_or(trimmed).trim();
            return key == "layout";
        }
    }
    false
}

/// Build the union offered in an indented statement slot — the grammatically-ambiguous position that
/// admits either a property key or a nested child widget (see [`Context::IndentedSlot`]). Following
/// consolidated LSPs, we return **all** valid candidates tagged by [`CompletionKind`] and ranked via
/// `sort_text`, and let the client's filter + icons disambiguate as the user types:
///
/// 1. the enclosing widget's **per-widget properties** — Lua-declared plus native C++
///    `onStyleApply` tags (most specific → ranked first), then
/// 2. the global C++ **property catalog**, then
/// 3. **child-widget names** — workspace `.otui` styles, Lua widget classes, and the native `UI*`
///    bases in use.
///
/// De-duplicated by label. The `sort_text` prefixes (`0_`/`1_`/`2_`) impose the group order; within a
/// group the client sorts by the appended label. The natural case convention still does the work for
/// free: catalog/Lua properties are lowercase, widget names are capitalized, so typing one letter
/// filters to the intended family.
fn indented_slot_items(
    tree: Option<&SyntaxTree>,
    source: &str,
    offset: usize,
    styles: &StyleIndex,
    lua: &LuaWidgetIndex,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |label: String,
                    kind: CompletionKind,
                    detail: &str,
                    group: char,
                    insert_text: Option<String>,
                    documentation: Option<String>| {
        if seen.insert(label.clone()) {
            let sort_text = Some(format!("{group}_{label}"));
            let insert_format = if insert_text.is_some() {
                InsertFormat::Snippet
            } else {
                InsertFormat::Plain
            };
            items.push(CompletionItem {
                label,
                kind,
                detail: Some(detail.to_owned()),
                sort_text,
                insert_text,
                insert_format,
                documentation,
            });
        }
    };

    // 1. The enclosing widget's per-widget properties (ranked first — most specific to this widget):
    // the union of its Lua-declared custom properties and its native C++ `onStyleApply` tags (see
    // `WidgetAncestry::custom_properties`) — the two origins are merged into one flat set there, so
    // they cannot be told apart here either; "widget property" covers both.
    if let Some(widget) = tree.and_then(|t| enclosing_widget_type_at(t, source, offset)) {
        let ancestry = widget_resolve::resolve_ancestry(&widget, styles, lua);
        for prop in ancestry.custom_properties(lua) {
            let insert = Some(property_key_snippet(&prop));
            push(
                prop,
                CompletionKind::Keyword,
                "widget property",
                '0',
                insert,
                None,
            );
        }
    }
    // 2. The global C++ property catalog, documented from the curated property-hover notes.
    for &prop in catalog::PROPERTIES {
        let insert = Some(property_key_snippet(prop));
        push(
            prop.to_owned(),
            CompletionKind::Keyword,
            "property",
            '1',
            insert,
            property_hover::documentation_body(prop),
        );
    }
    // 3. Child-widget names in scope across the workspace. Each carries a snippet: the widget name
    // plus an indented `id:` skeleton one level deeper than the current slot, tab-stopped so the id
    // value and "what's next" are typed without extra keystrokes.
    let child_indent = format!("{}  ", line_indent(source, offset));
    for (name, detail) in widget_names(styles, lua) {
        let insert = Some(child_widget_snippet(&name, &child_indent));
        push(name, CompletionKind::Class, detail, '2', insert, None);
    }
    items
}

/// The snippet body for completing a property **key**: the key, a colon-space, and a final tab-stop
/// for the value — `key: $0`. Saves the colon-space keystrokes on every property, the single biggest
/// cheap win here. `key` is schema/catalog/Lua-property text, so it is snippet-escaped defensively
/// (see [`snippet_escape`]) even though none of today's names carry a literal `$`/`}`/`\`.
fn property_key_snippet(key: &str) -> String {
    format!("{}: $0", snippet_escape(key))
}

/// The snippet body for completing a child **widget**: the widget name, then one line indented a
/// level deeper (`child_indent`) with an `id:` tab-stop, then a final tab-stop back at `child_indent`
/// for whatever comes next inside the new widget.
fn child_widget_snippet(label: &str, child_indent: &str) -> String {
    format!(
        "{}\n{child_indent}id: $1\n{child_indent}$0",
        snippet_escape(label)
    )
}

/// The leading run of spaces on the line containing `offset` (the current line's indentation). By
/// the time this runs, [`classify`] has already rejected tab-indented lines, so this is always pure
/// spaces. Used to compute one level deeper (`+ "  "`) for a nested snippet.
fn line_indent(source: &str, offset: usize) -> &str {
    let line_start = source[..offset].rfind('\n').map_or(0, |nl| nl + 1);
    let line = &source[line_start..];
    &line[..line.len() - line.trim_start_matches(' ').len()]
}

/// Escape the LSP/TextMate snippet metacharacters (`\`, `$`, `}`) in `text` so it is inserted
/// literally when embedded in a snippet body. OTML property values and identifiers routinely start
/// with `$` (a `$state` selector, a `$variable` reference) — a real hazard, not a theoretical one, if
/// any data-derived text (a property/widget name from the catalog, schema or a scanned Lua widget)
/// ever flowed unescaped into a snippet: the client would read a stray `$` as the start of its own
/// tab-stop syntax instead of pasting it literally.
fn snippet_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '$' | '}') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Build the items offered in a `Name < Base` style header's **base slot** ([`Context::BaseSlot`]):
/// the same valid-base union `widget_names` builds for the child-widget slot — every workspace
/// `.otui` style name plus the `UI*` native classes in use (spec §6, §2.2; engine ground:
/// `UIManager::getStyle`, `src/framework/ui/uimanager.cpp:527-545`, which resolves a style's base
/// against exactly that set). Unlike [`indented_slot_items`]'s child-widget entries, a base is a
/// plain type reference — not a nested statement — so each item is a **bare label**, with no
/// `id:`-skeleton snippet.
fn base_slot_items(styles: &StyleIndex, lua: &LuaWidgetIndex) -> Vec<CompletionItem> {
    widget_names(styles, lua)
        .into_iter()
        .map(|(label, detail)| CompletionItem {
            label,
            kind: CompletionKind::Class,
            detail: Some(detail.to_owned()),
            sort_text: None,
            insert_text: None,
            insert_format: InsertFormat::Plain,
            documentation: None,
        })
        .collect()
}

/// The widget/style type names usable as a nested child tag, each with a short origin `detail`:
/// workspace `.otui` styles and the native `UI*` bases they reference ([`StyleIndex`]), plus the Lua
/// widget classes ([`LuaWidgetIndex`]). Returned sorted by name for determinism; the caller
/// de-duplicates across the sources (a native referenced as a base and defined in Lua collapses).
fn widget_names(styles: &StyleIndex, lua: &LuaWidgetIndex) -> Vec<(String, &'static str)> {
    use crate::style_index::is_native_base;
    let mut named: std::collections::BTreeMap<String, &'static str> =
        std::collections::BTreeMap::new();
    for (_, def) in styles.iter() {
        named.entry(def.name.clone()).or_insert("style");
        if let Some(base) = def.base.as_deref()
            && is_native_base(base)
        {
            named.entry(base.to_owned()).or_insert("native widget");
        }
    }
    for def in lua.iter() {
        named.entry(def.name.clone()).or_insert("lua widget");
        // A widget's native `extends` parent is itself a valid child type — this is how the built-in
        // `UI*` classes (`UIWidget`, `UIButton`, `UIScrollArea`, …) surface as completions without a
        // hardcoded, fork-specific list: they are the parents the scanned corelib/gamelib widgets
        // derive from, captured dynamically. (A `UI*` base referenced by an `.otui` style is likewise
        // captured above.)
        if let Some(parent) = def.lua_parent.as_deref()
            && is_native_base(parent)
        {
            named.entry(parent.to_owned()).or_insert("native widget");
        }
    }
    named.into_iter().collect()
}

/// The type name of the widget enclosing the cursor at `offset` (the type a property on that widget
/// resolves against, mirroring the diagnostics pass): the completion-specific entry point onto the
/// shared [`widget_resolve::enclosing_widget_type`]. `None` when the source cannot be parsed or the
/// cursor has no enclosing widget. Best-effort mid-edit: the current line is often a broken `ERROR`
/// region, but the enclosing widget itself is usually intact, so its type still resolves.
///
/// Passes the cursor's line start as the mid-edit skip threshold: the half-typed key on the cursor's
/// own line frequently parses as a bare `container` tag (a lowercase word with no `:` yet is
/// grammatically a widget tag), which is **not** the enclosing widget — it is the property being
/// typed. So any candidate widget that starts on the cursor's line is skipped; only a widget declared
/// on an earlier line is a genuine encloser.
fn enclosing_widget_type_at(tree: &SyntaxTree, source: &str, offset: usize) -> Option<String> {
    let line_start = source[..offset].rfind('\n').map_or(0, |nl| nl + 1);
    let lo = offset.saturating_sub(1);
    let node = tree.root().descendant_for_byte_range(lo, offset)?;
    widget_resolve::enclosing_widget_type(node, source, Some(line_start))
}

/// Classify the closed-set context from the line `prefix` (line start up to the cursor).
///
/// Precise by construction: it only returns a context when the prefix unambiguously matches one of
/// the grammar shapes (a `$state` / `anchors.` / `@event` special form, or a bare property `key`) AND
/// the cursor is actively building the relevant token; anything else (a property **value** after the
/// `:`, a completed token followed by whitespace, a tab-indented line, …) yields `None`.
fn classify(prefix: &str) -> Option<Context> {
    // Tab indentation is a hard OTML parse error (the engine rejects it — see the `tab-indentation`
    // diagnostic), so a tab-indented line is never valid markup: offer nothing on it.
    let indent = &prefix[..prefix.len() - prefix.trim_start().len()];
    if indent.contains('\t') {
        return None;
    }

    // Indentation is spaces; strip it to see the first meaningful char. (A leading `$`/`@`/`anchors`
    // is only ever at the token start of a line, after indentation.)
    let trimmed = prefix.trim_start();

    // `@event` key: the line opens with `@`, the cursor is still inside the single event-name token
    // (no whitespace yet), and it has not reached its `:` (past which the value is embedded Lua —
    // out of scope). A completed name followed by a space (`@onClick `) is no longer being built.
    if let Some(rest) = trimmed.strip_prefix('@') {
        let building = !rest.contains(':') && !rest.contains(char::is_whitespace);
        return building.then_some(Context::Events);
    }

    // `$state` selector: the line opens with `$` and has not reached its `:`. States are
    // space-separated, so offer while the cursor is building a state token — right after the `$`
    // (empty selector), or with a non-empty current segment (a partial state or a `!` negation) —
    // but not when it sits after a completed state followed by whitespace (`$hover `). A `$var` in
    // value position never reaches here — it sits after a `key:`, so the trimmed prefix starts with
    // the key, not `$`.
    if let Some(rest) = trimmed.strip_prefix('$') {
        if rest.contains(':') {
            return None;
        }
        let current = rest.rsplit(char::is_whitespace).next().unwrap_or("");
        let building = rest.is_empty() || !current.is_empty();
        return building.then_some(Context::States);
    }

    // `anchors.<edge>: <target>`: only the literal `anchors.` object counts (a generic dotted key
    // like `foo.left:` is not an anchor, per the grammar).
    if trimmed.starts_with("anchors.") {
        return match trimmed.find(':') {
            // Before the `:` → the edge key slot.
            None => Some(Context::AnchorEdgeKey),
            // After the `:` → the value slot.
            Some(colon) => classify_anchor_value(&trimmed[colon + 1..]),
        };
    }

    // A `key: value` property with the cursor past the `:` → the value position for that key. The
    // engine splits on the first `:`, so the key is everything before it. Only an indented line is a
    // property (a top-level word is a `Name < Base` header). Which values (if any) apply is resolved
    // downstream from the property's value kind.
    if let Some(colon) = trimmed.find(':') {
        if indent.is_empty() {
            return None;
        }
        let value = trimmed[colon + 1..].trim_start();
        // A `$variable` reference in the value is not a literal from the property's fixed set, so
        // offer nothing there (the client is typing a variable name, not a `display`/color value).
        if value.starts_with('$') {
            return None;
        }
        // The offered value sets are single-token (a `display`/`layout` keyword, a color name), so
        // offer only while the first value token is being built — not after a completed value plus
        // whitespace, nor for a second token (mirrors the state / anchor-target slots).
        let building = match value.split_whitespace().count() {
            0 => true,
            1 => !value.ends_with(char::is_whitespace),
            _ => false,
        };
        if !building {
            return None;
        }
        return Some(Context::PropertyValue(
            trimmed[..colon].trim_end().to_owned(),
        ));
    }

    // The **base slot** of a top-level `Name < Base` style header (spec §6, §2.2): unindented, the
    // line carries a `<`, and it is not `#`-frozen (per otmlparser.cpp:311 a leading `#` makes the
    // WHOLE line a comment, so nothing on it is completable — the NAME side, before the `<`, is
    // likewise never completable: it is the author's own new name, not a reference). The base is
    // everything after the *last* `<` in the prefix, trimmed of its leading whitespace. Offer while
    // that segment is empty or a single in-progress identifier (letters/digits/`_`/`-`); `style_base`
    // (grammar.js:111) is one greedy whole-line token, so any interior whitespace or `:` in the
    // segment means a completed base is already followed by more content, and nothing further is
    // offered.
    if indent.is_empty() && !trimmed.starts_with('#') {
        if let Some(last_lt) = trimmed.rfind('<') {
            let segment = trimmed[last_lt + 1..].trim_start();
            let building = segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            return building.then_some(Context::BaseSlot);
        }
        // No `<` yet: the cursor is still on the NAME side (or the line has no header at all) —
        // neither is a completable slot.
        return None;
    }

    // An indented statement slot — a property key or a nested child widget being typed.
    classify_indented_slot(indent, trimmed)
}

/// Classify the **indented statement slot** from the line's `indent` and its `trimmed` content — the
/// position that admits either a property key or a nested child widget (see [`Context::IndentedSlot`]).
///
/// Fires [`Context::IndentedSlot`] when the cursor is on an indented line either empty or building a
/// bare identifier token (letters/digits/`_`/`-`, any leading case). The union — and the client's
/// filter — sort out property-vs-widget, so unlike the earlier precision-first design this no longer
/// stays silent on an empty slot or an uppercase word. Still `None` for:
///
/// * unindented (`indent` empty) → a top-level word is a style-header declaration, a separate concern;
/// * a `:` in the token → past the key, in **value** position (value completion is deferred);
/// * a `.` → a dotted key (`foo.bar`) is neither a property nor an `anchors.` object;
/// * a leading digit / `&` / `!` / `-` / other non-letter → an alias/expr/list form, not this slot;
/// * `<`, whitespace, or any other char in the token → a `Name < Base` header or a completed word.
///
/// The `$` / `@` / `anchors.` forms are already handled upstream. Indentation-depth validity is
/// enforced separately by the caller (it needs the whole document).
fn classify_indented_slot(indent: &str, trimmed: &str) -> Option<Context> {
    // Statements are nested under a widget, so they always carry leading indentation. A top-level
    // (unindented) bare word is a style-header declaration, out of scope here.
    if indent.is_empty() {
        return None;
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        // An empty indented slot: offer the whole union (property keys + child widgets).
        None => return Some(Context::IndentedSlot),
        // A word being typed must start with a letter (either case). A leading digit / `&` / `!` /
        // `-` / `.` marks an alias/expr/list/dotted form, not this slot.
        Some(first) if first.is_ascii_alphabetic() => {}
        Some(_) => return None,
    }
    // The rest must stay within the IDENT charset. A `:` (value position — deferred), `.` (dotted
    // key), `<` (style header), whitespace (completed word / multi-word tag), or any other char
    // means this is not a bare token still being typed.
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(Context::IndentedSlot)
}

/// Classify the anchor **value** slot — the text after `anchors.<edge>:` up to the cursor.
///
/// The value is a single target token, so offer only while the cursor is building it:
/// * an empty value slot (just past the `:`) → the target start → [`Context::AnchorTarget`],
/// * a non-empty current segment containing `.` → a target-side edge → [`Context::AnchorTargetEdge`],
/// * a non-empty current segment without `.` → still the target position → [`Context::AnchorTarget`],
/// * a completed token followed by whitespace (`parent `) → nothing.
fn classify_anchor_value(value: &str) -> Option<Context> {
    let current = value.rsplit(char::is_whitespace).next().unwrap_or("");
    if current.is_empty() {
        // Empty slot (just after `:`) offers the target start; a trailing space after a completed
        // token does not.
        return value.trim().is_empty().then_some(Context::AnchorTarget);
    }
    if current.contains('.') {
        Some(Context::AnchorTargetEdge)
    } else {
        Some(Context::AnchorTarget)
    }
}

/// Build the [`CompletionItem`]s for a classified [`Context`], in a stable order (schema const order
/// for the closed sets; magic targets then in-scope ids for the anchor target slot).
fn items_for(context: Context, source: &str, offset: usize) -> Vec<CompletionItem> {
    match context {
        Context::States => set_items(schema::STATES, CompletionKind::EnumMember, "state"),
        Context::AnchorEdgeKey => {
            // Both the edges and the shorthands sit on the key side of `anchors.<edge>: <target>`, so
            // completing either continues straight into the target slot: `top: $0`.
            let mut items = key_snippet_items(
                schema::ANCHOR_EDGES,
                CompletionKind::EnumMember,
                "anchor edge",
            );
            items.extend(key_snippet_items(
                schema::SHORTHAND_ANCHORS,
                CompletionKind::EnumMember,
                "anchor shorthand",
            ));
            items
        }
        // A target-side edge (`parent.top`) is the last token of the value — no `:`/value slot
        // follows it, so it stays a plain label.
        Context::AnchorTargetEdge => set_items(
            schema::ANCHOR_EDGES,
            CompletionKind::EnumMember,
            "anchor edge",
        ),
        Context::AnchorTarget => anchor_target_items(source, offset),
        // An `@event` key always continues into its handler body: `@onClick: $0`.
        Context::Events => {
            key_snippet_items(schema::EVENTS, CompletionKind::Event, "event handler")
        }
        // The indented statement slot is workspace-aware (catalog + Lua props + widget names), so it
        // is built by `indented_slot_items` and routed before this function is reached.
        Context::IndentedSlot => {
            unreachable!("IndentedSlot is handled in complete_at_with_widgets")
        }
        Context::PropertyValue(key) => property_value_items(&key),
        Context::LayoutBlock => layout_block_key_items(),
        Context::LayoutValue(key) => layout_value_items(&key),
        // The base slot is workspace-aware (the same `widget_names` union as the child-widget
        // slot), so it is built by `base_slot_items` and routed before this function is reached.
        Context::BaseSlot => unreachable!("BaseSlot is handled in complete_at_with_widgets"),
    }
}

/// The **key** completions inside a `layout:` block: the closed set of keys the layout classes read
/// ([`catalog::LAYOUT_PROPERTIES`]), each carrying the same `key: $0` snippet as an ordinary property
/// key. Unlike [`indented_slot_items`] this is not a union with child-widget names — a `layout:` block
/// holds only properties, never nested widgets (spec: it is dispatched by the layout object, which
/// reads flat key/value tags, not a widget tree) — and not workspace-aware, since the block's key set
/// is fixed by the engine rather than any Lua/style index.
fn layout_block_key_items() -> Vec<CompletionItem> {
    catalog::LAYOUT_PROPERTIES
        .iter()
        .map(|&key| CompletionItem {
            label: key.to_owned(),
            kind: CompletionKind::Keyword,
            detail: Some("layout property".to_owned()),
            sort_text: None,
            insert_text: Some(property_key_snippet(key)),
            insert_format: InsertFormat::Snippet,
            documentation: None,
        })
        .collect()
}

/// The **value** completions for a key inside a `layout:` block ([`schema::is_layout_block_property`]),
/// from the audited value kind shared with hover ([`property_hover::classify_layout_value`]): the same
/// 4 layout-type keywords as the leaf `layout: <type>` form for `type`, `true`/`false` for a boolean
/// key, or nothing for a free numeric/dimension key (`Integer`/`Size`) — never fabricated. See
/// `classify_layout_value`'s doc comment for the per-key engine citations.
fn layout_value_items(key: &str) -> Vec<CompletionItem> {
    match property_hover::classify_layout_value(key) {
        property_hover::PropertyValueKind::Enum { values } => {
            set_items(values, CompletionKind::EnumMember, "layout type")
        }
        property_hover::PropertyValueKind::Boolean => {
            set_items(&["true", "false"], CompletionKind::Value, "value")
        }
        _ => Vec::new(),
    }
}

/// The completion values for the value position of property `key`, from its audited value kind
/// ([`property_hover::classify_value`]): the `display`/`layout`/alignment/flexbox keyword sets, the
/// `true`/`false` pair for a boolean property, or the named-color list for a color property. A
/// freeform-valued property (a number, a path, the `border` shorthand, an arbitrary string, or an
/// unknown key) offers nothing.
fn property_value_items(key: &str) -> Vec<CompletionItem> {
    match property_hover::classify_value(key) {
        property_hover::PropertyValueKind::Enum { values } => {
            set_items(values, CompletionKind::EnumMember, "value")
        }
        property_hover::PropertyValueKind::Boolean => {
            set_items(&["true", "false"], CompletionKind::Value, "value")
        }
        property_hover::PropertyValueKind::Color => catalog::NAMED_COLORS
            .iter()
            .map(|(name, rgb)| CompletionItem {
                label: (*name).to_owned(),
                kind: CompletionKind::Value,
                detail: Some("named color".to_owned()),
                sort_text: None,
                insert_text: None,
                insert_format: InsertFormat::Plain,
                documentation: Some(format!("`{}`", packed_rgb_to_hex(*rgb))),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Format a packed `0xRRGGBB` value ([`catalog::NAMED_COLORS`]) as a lowercase `#rrggbb` hex string.
fn packed_rgb_to_hex(rgb: u32) -> String {
    format!("#{rgb:06x}")
}

/// Build the anchor **target** candidates: the magic pseudo-targets (`parent` / `next` / `prev`)
/// followed by the in-scope widget `id:` values reachable from the anchor's owning widget (spec §6).
///
/// Both carry [`CompletionKind::Value`]; the ids are tagged `"widget id"` to distinguish them in the
/// client. Magic targets come first (schema order), then ids in source order, de-duplicated by label
/// (an id literally named `parent` collapses into the magic entry).
fn anchor_target_items(source: &str, offset: usize) -> Vec<CompletionItem> {
    let mut items = set_items(
        schema::MAGIC_ANCHOR_TARGETS,
        CompletionKind::Value,
        "anchor target",
    );
    for id in sibling_anchor_ids(source, offset) {
        if items.iter().any(|item| item.label == id) {
            continue;
        }
        items.push(CompletionItem {
            label: id,
            kind: CompletionKind::Value,
            detail: Some("widget id".to_owned()),
            sort_text: None,
            insert_text: None,
            insert_format: InsertFormat::Plain,
            documentation: None,
        });
    }
    items
}

/// Collect the widget `id:` values reachable as concrete anchor targets from the widget owning the
/// anchor at `offset`: the owner's **direct sibling widgets** (same parent, excluding the owner
/// itself), in source order, de-duplicated. A purely **local** CST walk over the current document —
/// anchor targets resolve within one widget tree, never across files, so the cross-file style index
/// is intentionally not consulted. Returns empty on a parse failure or when the owner cannot be
/// located.
///
/// Engine rule: `UIAnchor::getHookedWidget` (uianchorlayout.cpp:26-42) resolves the magic
/// `parent`/`next`/`prev` targets and otherwise calls `parentWidget->getChildById(targetId)`, a
/// single non-recursive lookup in `m_childrenById` (uiwidget.cpp:1487-1494) that only ever holds
/// direct children. An ancestor's id can never satisfy that lookup, so ancestors are not offered
/// here (they would never resolve at runtime).
fn sibling_anchor_ids(source: &str, offset: usize) -> Vec<String> {
    let Some(tree) = SyntaxTree::parse(source) else {
        return Vec::new();
    };
    let root = tree.root();
    let Some(owner) = anchor_owner_widget(root, offset) else {
        return Vec::new();
    };

    // Source order, de-duplicated by id text.
    let mut ids = Vec::new();
    for widget in direct_sibling_widgets(owner) {
        if let Some(id) = widget_id(widget, source)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

/// The widget node that owns the anchor line under `offset`: walk up from the nearest node to the
/// nearest `container`/`style_header` ancestor. `None` on a parse failure the caller already handled,
/// or when no such ancestor exists. Shared by [`sibling_anchor_ids`] (completion) and
/// [`navigation::resolve_anchor_target`](crate::navigation::resolve_anchor_target) (hover /
/// go-to-definition) — both ask the identical "which widget does this anchor belong to?" question,
/// each from its own cursor position on the same `anchors.<edge>:` line.
pub(crate) fn anchor_owner_widget(root: Node<'_>, offset: usize) -> Option<Node<'_>> {
    let lo = offset.saturating_sub(1);
    let mut node = root.descendant_for_byte_range(lo, offset)?;
    loop {
        if is_widget(node) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// `owner`'s direct sibling widgets — same parent, excluding `owner` itself — in source order. The
/// exact in-scope set an anchor target may resolve against (see [`sibling_anchor_ids`]'s engine
/// citation); shared with
/// [`navigation::resolve_anchor_target`](crate::navigation::resolve_anchor_target), which additionally
/// needs each sibling's own widget kind and `id:` span, not just its id text.
///
/// Engine rule: a top-level `Name < Base` header — one whose parent is the grammar's `document`
/// root — is a **global style definition** registered in `UIManager::m_styles`
/// (`UIManager::importStyleFromOTML`, uimanager.cpp:467-514; dispatched from `loadUI` at
/// :612/:625-629, where only one top-level node without `<` becomes the main widget). It is never
/// added to any widget's child list, so `UIWidget::getChildById` (uiwidget.cpp:1487, which only
/// ever searches that instance's own `m_childrenById`) can never resolve one top-level style's id
/// from another. Such an owner has no anchor-resolvable siblings at all, so this returns empty
/// rather than the other `document`-level entries the raw CST walk would otherwise offer.
pub(crate) fn direct_sibling_widgets(owner: Node<'_>) -> Vec<Node<'_>> {
    let mut scope: Vec<Node> = Vec::new();
    if let Some(parent) = owner.parent()
        && parent.kind() != "document"
    {
        let mut cursor = parent.walk();
        for sibling in parent.named_children(&mut cursor) {
            if is_widget(sibling) && sibling.id() != owner.id() {
                scope.push(sibling);
            }
        }
    }
    scope.sort_by_key(Node::start_byte);
    scope
}

/// Whether `node` is a widget node (a bare `container` tag or a `Name < Base` `style_header`).
pub(crate) fn is_widget(node: Node) -> bool {
    matches!(node.kind(), "container" | "style_header")
}

/// The `id:` value declared directly on `widget`, if any (its `id_property` child's value text,
/// trimmed). `None` when the widget declares no id.
pub(crate) fn widget_id(widget: Node, source: &str) -> Option<String> {
    widget_id_ref(widget, source).map(|(id, _)| id)
}

/// Like [`widget_id`], but also returns the trimmed value's own byte span — the `id:` declaration's
/// exact location, needed by
/// [`navigation::resolve_anchor_target`](crate::navigation::resolve_anchor_target) to build a
/// go-to-definition target. `None` when the widget declares no id.
pub(crate) fn widget_id_ref(widget: Node, source: &str) -> Option<(String, lang_api::ByteSpan)> {
    let mut cursor = widget.walk();
    for child in widget.named_children(&mut cursor) {
        if child.kind() != "id_property" {
            continue;
        }
        let value = child.child_by_field_name("value")?;
        let raw_start = value.start_byte();
        let raw = &source[raw_start..value.end_byte()];
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let start = raw_start + (raw.len() - raw.trim_start().len());
        let end = start + trimmed.len();
        return Some((trimmed.to_owned(), lang_api::ByteSpan::new(start, end)));
    }
    None
}

/// Map a schema const slice to completion items with a shared `kind` and `detail`, preserving the
/// slice's order. No snippet — plain-label insertion.
fn set_items(set: &[&str], kind: CompletionKind, detail: &str) -> Vec<CompletionItem> {
    set.iter()
        .map(|&label| CompletionItem {
            label: label.to_owned(),
            kind,
            detail: Some(detail.to_owned()),
            sort_text: None,
            insert_text: None,
            insert_format: InsertFormat::Plain,
            documentation: None,
        })
        .collect()
}

/// Like [`set_items`], but each item's insert text continues straight into the value slot the key
/// always opens onto: `{label}: $0`. Used for the `@event` and `anchors.<edge>` closed sets, whose
/// grammar shape (`key: value`) is fixed the moment the key is chosen.
///
/// Each item's `documentation` comes from the shared [`property_hover::documentation_body`]: this is
/// `Some` for an anchor edge/shorthand label (`top`, `fill`, …) and `None` for an `@event` name (no
/// curated note exists for those; see the module docs).
fn key_snippet_items(set: &[&str], kind: CompletionKind, detail: &str) -> Vec<CompletionItem> {
    set.iter()
        .map(|&label| CompletionItem {
            label: label.to_owned(),
            kind,
            detail: Some(detail.to_owned()),
            sort_text: None,
            insert_text: Some(property_key_snippet(label)),
            insert_format: InsertFormat::Snippet,
            documentation: property_hover::documentation_body(label),
        })
        .collect()
}

/// True if `offset` sits inside a raw region where OTML completion must not fire — a `|` block-scalar
/// body or a full-line comment. Best-effort: a parse failure (practically unreachable) is treated as
/// "not suppressed" so the prefix classifier still runs.
fn in_suppressed_context(tree: &SyntaxTree, offset: usize) -> bool {
    // Probe the byte just before the cursor (the char last typed) through the cursor, so a cursor at
    // the very end of a block-scalar line still lands inside its content node.
    let lo = offset.saturating_sub(1);
    let Some(mut node) = tree.root().descendant_for_byte_range(lo, offset) else {
        return false;
    };
    loop {
        if matches!(node.kind(), "block_scalar_content" | "comment") {
            return true;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte offset of the first occurrence of `needle` in `src` (panics if absent).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).expect("needle present")
    }

    /// The labels of the returned items, in order — the determinism surface the tests assert on.
    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn cursor_after_dollar_offers_all_states() {
        let src = "Button\n  $\n";
        let offset = at(src, "$") + 1; // just past the `$`
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::STATES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn partial_state_word_still_offers_the_whole_set() {
        // The client filters by the partial word; the engine returns the full set deterministically.
        let src = "Button\n  $ho\n";
        let offset = at(src, "$ho") + 3;
        assert_eq!(labels(&complete_at(src, offset)), schema::STATES);
    }

    #[test]
    fn cursor_after_negation_in_a_multi_state_selector_offers_states() {
        let src = "Button\n  $hover !\n";
        let offset = at(src, "!") + 1;
        assert_eq!(labels(&complete_at(src, offset)), schema::STATES);
    }

    #[test]
    fn cursor_after_anchors_dot_offers_edges_and_shorthands() {
        let src = "Widget\n  anchors.\n";
        let offset = at(src, "anchors.") + "anchors.".len();
        let mut expected: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        expected.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(labels(&complete_at(src, offset)), expected);
        assert!(
            complete_at(src, offset)
                .iter()
                .all(|i| i.kind == CompletionKind::EnumMember)
        );
    }

    #[test]
    fn cursor_at_anchor_value_start_offers_magic_targets() {
        let src = "Widget\n  anchors.top: \n";
        let offset = at(src, "anchors.top: ") + "anchors.top: ".len();
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::MAGIC_ANCHOR_TARGETS);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
    }

    #[test]
    fn cursor_after_target_dot_in_anchor_value_offers_edges() {
        let src = "Widget\n  anchors.top: parent.\n";
        let offset = at(src, "parent.") + "parent.".len();
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::ANCHOR_EDGES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn cursor_after_at_offers_events() {
        let src = "Button\n  @\n";
        let offset = at(src, "@") + 1;
        let items = complete_at(src, offset);
        assert_eq!(labels(&items), schema::EVENTS);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Event));
    }

    #[test]
    fn partial_event_word_still_offers_the_whole_set() {
        let src = "Button\n  @onCl\n";
        let offset = at(src, "@onCl") + "@onCl".len();
        assert_eq!(labels(&complete_at(src, offset)), schema::EVENTS);
    }

    #[test]
    fn freeform_property_value_offers_nothing() {
        // A freeform value (an arbitrary string) has no closed set to complete.
        let src = "Widget\n  text: hel\n";
        assert!(complete_at(src, at(src, "hel") + 1).is_empty());
    }

    #[test]
    fn value_position_for_display_offers_the_display_values() {
        let src = "Widget\n  display: fl\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "fl") + 2)),
            schema::DISPLAY_VALUES
        );
    }

    #[test]
    fn value_position_right_after_the_colon_offers_the_whole_set() {
        let src = "Widget\n  layout:\n";
        let off = at(src, "layout:") + "layout:".len();
        assert_eq!(labels(&complete_at(src, off)), schema::LAYOUT_TYPES);
    }

    // --- `layout:` block completion --------------------------------------------------------------

    #[test]
    fn key_slot_inside_a_layout_block_offers_the_layout_property_set() {
        // Cursor building a bare key nested under a `layout:` header: the block's own closed key
        // set (never the global catalog nor child-widget names — a layout block holds no widgets).
        let src = "Panel\n  layout:\n    fit\n";
        let items = complete_at(src, at(src, "fit") + "fit".len());
        assert_eq!(labels(&items), catalog::LAYOUT_PROPERTIES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Keyword));
        assert!(
            items
                .iter()
                .all(|i| i.detail.as_deref() == Some("layout property"))
        );
    }

    #[test]
    fn empty_key_slot_inside_a_layout_block_offers_the_layout_property_set() {
        // The cursor sits at the start of an indented line that already has a word after it — an
        // empty slot the classifier opens (mirrors `empty_slot_before_existing_content_offers_the_union`)
        // — inside a `layout:` block this still offers the block's own key set.
        let src = "Panel\n  layout:\n    fit\n";
        let off = at(src, "fit"); // cursor right before the word
        assert_eq!(labels(&complete_at(src, off)), catalog::LAYOUT_PROPERTIES);
    }

    #[test]
    fn layout_block_key_snippet_continues_into_the_value_slot() {
        let src = "Panel\n  layout:\n    fit\n";
        let items = complete_at(src, at(src, "fit") + "fit".len());
        let fit = items
            .iter()
            .find(|i| i.label == "fit-children")
            .expect("fit-children offered");
        assert_eq!(fit.insert_text.as_deref(), Some("fit-children: $0"));
        assert_eq!(fit.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn layout_block_type_value_offers_the_four_layout_types() {
        let src = "Panel\n  layout:\n    type: ver\n";
        let items = complete_at(src, at(src, "ver") + "ver".len());
        assert_eq!(labels(&items), schema::LAYOUT_TYPES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::EnumMember));
    }

    #[test]
    fn layout_block_boolean_keys_offer_true_and_false() {
        for key in [
            "fit-children",
            "align-right",
            "align-bottom",
            "auto-spacing",
            "flow",
        ] {
            let src = format!("Panel\n  layout:\n    {key}: t\n");
            let items = complete_at(&src, at(&src, "t\n") + 1);
            assert_eq!(labels(&items), ["true", "false"], "{key}");
        }
    }

    #[test]
    fn layout_block_numeric_key_offers_no_value() {
        // `spacing`, the cell-* keys and num-columns/num-lines are free numeric/dimension input —
        // never fabricated.
        for key in [
            "spacing",
            "cell-size",
            "cell-width",
            "cell-height",
            "cell-spacing",
            "num-columns",
            "num-lines",
        ] {
            let src = format!("Panel\n  layout:\n    {key}: 1\n");
            let items = complete_at(&src, at(&src, "1\n") + 1);
            assert!(items.is_empty(), "{key} must offer no value completion");
        }
    }

    #[test]
    fn layout_shorthand_value_is_unaffected_by_the_block_context() {
        // Regression: `layout: verticalBox` on the header line itself is the leaf shorthand form,
        // not a block key — it must keep offering the layout-type set exactly as before, never the
        // layout-block key set.
        let src = "Widget\n  layout: ver\n";
        let items = complete_at(src, at(src, "ver") + "ver".len());
        assert_eq!(labels(&items), schema::LAYOUT_TYPES);
    }

    #[test]
    fn layout_block_child_at_a_sibling_property_indent_is_not_mistaken_for_the_block() {
        // A property at the SAME indentation as `layout:` (a sibling, not a child) must not be
        // routed into the layout-block key set.
        let src = "Panel\n  layout:\n    type: grid\n  wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        assert_eq!(labels(&items), catalog::PROPERTIES);
    }

    #[test]
    fn nested_widget_after_a_layout_block_is_not_mistaken_for_the_block() {
        let src = "Panel\n  layout:\n    type: grid\n  Child\n    wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        assert_eq!(labels(&items), catalog::PROPERTIES);
    }

    #[test]
    fn a_layout_header_with_both_a_shorthand_value_and_block_children_still_opens_the_block() {
        // The engine applies a `layout:` node's children even when it already has a leaf value
        // (`m_layout->applyStyle(node)` runs whenever the node has children, unconditionally) — so
        // this less-common authoring form must still be recognized as the block header.
        let src = "Panel\n  layout: verticalBox\n    spa\n";
        let items = complete_at(src, at(src, "spa") + "spa".len());
        assert_eq!(labels(&items), catalog::LAYOUT_PROPERTIES);
    }

    #[test]
    fn value_position_for_a_boolean_property_offers_true_and_false() {
        let src = "Widget\n  enabled:\n";
        let off = at(src, "enabled:") + "enabled:".len();
        let items = complete_at(src, off);
        assert_eq!(labels(&items), ["true", "false"]);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
    }

    #[test]
    fn value_position_for_the_flexbox_and_alignment_enums_offers_their_keywords() {
        let src = "Widget\n  text-align: ce\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "ce\n") + 2)),
            schema::ALIGNMENT_VALUES
        );
        let src = "Widget\n  overflow: hi\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "hi\n") + 2)),
            schema::OVERFLOW_VALUES
        );
    }

    #[test]
    fn value_position_for_the_second_batch_of_keyword_enums_offers_their_keywords() {
        // One completion round-trip per newly-wired property, each checked against its authored set.
        let cases: &[(&str, &'static [&'static str])] = &[
            ("flex-wrap: no", schema::FLEX_WRAP_VALUES),
            ("align-content: ce", schema::ALIGN_CONTENT_VALUES),
            ("align-self: au", schema::ALIGN_SELF_VALUES),
            ("position: ab", schema::POSITION_VALUES),
            ("float: le", schema::FLOAT_VALUES),
            ("clear: bo", schema::CLEAR_VALUES),
            ("justify-items: ce", schema::JUSTIFY_ITEMS_VALUES),
            ("auto-focus: fi", schema::AUTO_FOCUS_VALUES),
        ];
        for (line, values) in cases {
            let src = format!("Widget\n  {line}\n");
            let offset = at(&src, line) + line.len();
            assert_eq!(labels(&complete_at(&src, offset)), *values, "{line}");
        }
    }

    #[test]
    fn an_unverified_property_still_offers_no_values() {
        // `min-width` is a known catalog property but carries no audited value kind: it must not
        // spuriously gain a completion set.
        let src = "Widget\n  min-width: ab\n";
        assert!(complete_at(src, at(src, "ab\n") + 2).is_empty());
    }

    #[test]
    fn value_position_for_a_color_property_offers_named_colors() {
        let src = "Widget\n  color: re\n";
        let items = complete_at(src, at(src, "re\n") + 2);
        assert!(
            items.iter().any(|i| i.label == "red"),
            "named colors offered"
        );
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
        // A different color property (background-color) also offers colors.
        let src2 = "Widget\n  background-color: wh\n";
        assert!(
            complete_at(src2, at(src2, "wh") + 2)
                .iter()
                .any(|i| i.label == "white")
        );
    }

    #[test]
    fn value_completion_honors_the_indentation_gate() {
        // A 3-space (odd) indent is a hard OTML error; no value is offered on such a line.
        let src = "Widget\n   display: fl\n";
        assert!(complete_at(src, at(src, "fl") + 2).is_empty());
    }

    #[test]
    fn event_value_position_offers_nothing() {
        // After the `@event:` colon the value is embedded Lua — explicitly out of scope.
        let src = "Button\n  @onClick: foo\n";
        assert!(complete_at(src, at(src, "foo") + 1).is_empty());
    }

    #[test]
    fn generic_dotted_key_is_not_an_anchor() {
        // Only the literal `anchors.` object is an anchor; `foo.left:` is a generic property.
        let src = "Widget\n  foo.\n";
        assert!(complete_at(src, at(src, "foo.") + "foo.".len()).is_empty());
    }

    #[test]
    fn value_dollar_reference_offers_nothing() {
        // A `$var` value is a variable reference, not a literal from the property's fixed set, so the
        // value branch suppresses it (even for a color property that would otherwise offer colors).
        let src = "Widget\n  color: $pri\n";
        assert!(complete_at(src, at(src, "$pri") + "$pri".len()).is_empty());
    }

    #[test]
    fn a_completed_value_followed_by_whitespace_offers_nothing() {
        // The value sets are single-token, so after a completed value the branch stops offering —
        // no re-offering of the whole set on a trailing space or a second token.
        let src = "Widget\n  color: red \n";
        assert!(complete_at(src, at(src, "red ") + "red ".len()).is_empty());
        let src2 = "Widget\n  color: red gr\n";
        assert!(complete_at(src2, at(src2, "gr\n") + 2).is_empty());
    }

    #[test]
    fn inside_a_block_scalar_body_offers_nothing() {
        // A `|` block body is raw Lua: a line starting with `$` there is not a state selector.
        let src = "Button\n  @onClick: |\n    $foo\n";
        let offset = at(src, "$foo") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn inside_a_full_line_comment_offers_nothing() {
        // A line-start `//` (or `#`) comment is prose, not markup: a `$`/`@` inside it must not
        // trigger a closed-set completion (the CST-suppression path for `comment`).
        let src = "Button\n  // $hover\n";
        let offset = at(src, "$hover") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn tab_indented_line_offers_nothing() {
        // Tab indentation is a hard OTML parse error; no closed set may be offered on such a line.
        let src = "Button\n\t$hover\n";
        let offset = at(src, "$") + 1;
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn complete_event_name_followed_by_space_offers_nothing() {
        // `@onClick ` — the event name is complete and the cursor sits after a space (no `:` yet);
        // the token is no longer being built, so nothing is offered.
        let src = "Button\n  @onClick \n";
        let offset = at(src, "@onClick ") + "@onClick ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn complete_anchor_target_followed_by_space_offers_nothing() {
        // `anchors.top: parent ` — the target is complete and the cursor sits after a trailing
        // space; the target token is no longer being built, so nothing is offered.
        let src = "Widget\n  anchors.top: parent \n";
        let offset = at(src, "parent ") + "parent ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn still_building_tokens_keep_offering_their_sets() {
        // Partial tokens (the cursor actively building them) still get the whole set.
        let src = "Button\n  @onCl\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "@onCl") + "@onCl".len())),
            schema::EVENTS
        );

        let src = "Button\n  $hov\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "$hov") + "$hov".len())),
            schema::STATES
        );

        // `anchors.to` (partial edge, no colon) still offers edges + shorthands.
        let src = "Widget\n  anchors.to\n";
        let mut edges: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        edges.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(
            labels(&complete_at(
                src,
                at(src, "anchors.to") + "anchors.to".len()
            )),
            edges
        );

        // `anchors.top: par` (partial target) still offers the target set (magic targets here — no
        // ids in this doc).
        let src = "Widget\n  anchors.top: par\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "par\n") + "par".len())),
            schema::MAGIC_ANCHOR_TARGETS
        );
    }

    #[test]
    fn anchor_target_without_ids_offers_only_magic_targets() {
        // A document with no widget ids: the target slot offers just `parent`/`next`/`prev`.
        let src = "Widget\n  anchors.top: \n";
        let offset = at(src, "anchors.top: ") + "anchors.top: ".len();
        assert_eq!(
            labels(&complete_at(src, offset)),
            schema::MAGIC_ANCHOR_TARGETS
        );
    }

    #[test]
    fn anchor_target_offers_direct_siblings_not_ancestors() {
        // Button owns the anchor. `UIAnchor::getHookedWidget` resolves a concrete target only via
        // `parentWidget->getChildById`, a non-recursive lookup over direct children — so only
        // Button's sibling `Label` (id `lbl`) is offered. Its ancestor `Panel` (id `root`) can never
        // be reached that way and must NOT be offered; Button's own id `btn` is excluded too (a
        // widget cannot anchor to itself).
        let src = "\
Panel
  id: root
  Button
    id: btn
    anchors.top:
  Label
    id: lbl
";
        let offset = at(src, "anchors.top:") + "anchors.top:".len();
        let mut expected: Vec<String> = schema::MAGIC_ANCHOR_TARGETS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        expected.push("lbl".to_owned());
        assert_eq!(labels(&complete_at(src, offset)), expected);
        // Button's own id and its ancestor's id are not targets; magic targets stay `Value`, ids
        // are tagged `widget id`.
        let items = complete_at(src, offset);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Value));
        assert!(!items.iter().any(|i| i.label == "btn"));
        assert!(!items.iter().any(|i| i.label == "root"));
        assert_eq!(
            items
                .iter()
                .find(|i| i.label == "lbl")
                .and_then(|i| i.detail.as_deref()),
            Some("widget id")
        );
    }

    #[test]
    fn anchor_target_top_level_style_headers_are_not_siblings_of_each_other() {
        // Two top-level `Name < Base` headers: each is a *global style definition* registered in
        // `UIManager::m_styles` (`UIManager::importStyleFromOTML`), never added to any widget's
        // child list — so `First`'s anchor can never resolve `Second`'s id `other` via
        // `getChildById`, even though the raw CST walk would otherwise see them as siblings under
        // the shared `document` root. Only the magic targets are offered.
        let src = "\
First < UIWidget
  anchors.top:
Second < UIWidget
  id: other
";
        let offset = at(src, "anchors.top:") + "anchors.top:".len();
        assert_eq!(
            labels(&complete_at(src, offset)),
            schema::MAGIC_ANCHOR_TARGETS
        );
        let items = complete_at(src, offset);
        assert!(!items.iter().any(|i| i.label == "other"));
    }

    #[test]
    fn offscreen_or_unknown_offset_offers_nothing() {
        assert!(complete_at("", 0).is_empty());
        let src = "Button\n  $\n";
        assert!(complete_at(src, src.len() + 100).is_empty());
    }

    #[test]
    fn indented_partial_property_word_offers_the_catalog() {
        // `  wid` — a bare property key being typed on an indented line: offer all catalog
        // property names (the client filters by `wid` → `width`). Deterministic, catalog order.
        let src = "Button\n  wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        assert_eq!(labels(&items), catalog::PROPERTIES);
        assert!(items.iter().all(|i| i.kind == CompletionKind::Keyword));
        assert!(
            items
                .iter()
                .all(|i| i.detail.as_deref() == Some("property"))
        );
    }

    #[test]
    fn catalog_property_completion_carries_its_curated_documentation() {
        // `width` has a curated one-line note in `property_hover::PROPERTY_DOCS`; the completion
        // item must surface it, not just the one-word `detail`.
        let src = "Button\n  wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        let width = items.iter().find(|i| i.label == "width").expect("width");
        let doc = width.documentation.as_deref().expect("width has a doc");
        assert!(
            doc.contains("dimension"),
            "expected width's curated doc, got {doc:?}"
        );
    }

    #[test]
    fn enum_valued_property_completion_documentation_lists_its_values() {
        // `display` is both curated AND enum-valued: its documentation must end with the same
        // "One of: `a`, `b`, …" line the server's property-key hover renders for the same data.
        let src = "Button\n  disp\n";
        let items = complete_at(src, at(src, "disp") + "disp".len());
        let display = items
            .iter()
            .find(|i| i.label == "display")
            .expect("display");
        let doc = display.documentation.as_deref().expect("display has a doc");
        assert!(
            doc.contains("One of:"),
            "expected an enum list, got {doc:?}"
        );
        for value in schema::DISPLAY_VALUES {
            assert!(
                doc.contains(&format!("`{value}`")),
                "expected `{value}` listed in {doc:?}"
            );
        }
    }

    #[test]
    fn a_known_property_outside_the_curated_set_still_gets_the_validation_note() {
        // `min-width` is known but neither curated nor enum-valued (mirrors
        // `property_hover`'s equivalent test): a known property always gets a body from the shared
        // formatter now, at minimum the "silently ignored" validation note (`min-width` is not one
        // of the validating families).
        let src = "Button\n  min-wid\n";
        let items = complete_at(src, at(src, "min-wid") + "min-wid".len());
        let min_width = items
            .iter()
            .find(|i| i.label == "min-width")
            .expect("min-width");
        let doc = min_width
            .documentation
            .as_deref()
            .expect("min-width has a doc");
        assert!(doc.contains("silently ignored"), "{doc}");
    }

    #[test]
    fn named_color_completion_documentation_carries_its_hex_value() {
        // A named-color value item's documentation is the color's packed hex, not prose.
        let src = "Widget\n  color: re\n";
        let items = complete_at(src, at(src, "re\n") + 2);
        let red = items.iter().find(|i| i.label == "red").expect("red");
        assert_eq!(red.documentation.as_deref(), Some("`#ff0000`"));
    }

    #[test]
    fn property_key_context_is_deterministic() {
        // The returned labels equal exactly the catalog set, in const (sorted) order.
        let src = "Button\n  colo\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "colo") + "colo".len())),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn wholly_blank_indented_line_offers_nothing() {
        // A line that is *entirely* whitespace is treated as blank by the indentation validity gate
        // (it carries no structural depth), so completion stays silent there. The union opens up as
        // soon as the line has content — a typed word, or an empty slot before an existing word
        // (see `empty_slot_before_existing_content_offers_the_union`).
        let src = "Button\n  \n";
        let offset = at(src, "  \n") + 2; // just past the two indent spaces
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn completed_property_key_and_colon_offers_nothing() {
        // `  width:` — past the `:` we are in VALUE position; property-value/color completion is
        // deferred (needs per-property type metadata), so nothing is offered.
        let src = "Button\n  width:\n";
        let offset = at(src, "width:") + "width:".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn completed_property_word_followed_by_space_offers_nothing() {
        // `  width ` — the key is complete and the cursor sits after a space (no `:` yet); the token
        // is no longer being built, so nothing is offered.
        let src = "Button\n  width \n";
        let offset = at(src, "width ") + "width ".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn unindented_top_level_word_offers_nothing() {
        // `Button` at column 0 is a widget/style-header, NOT a property → offer nothing.
        let src = "Butt\n";
        assert!(complete_at(src, at(src, "Butt") + "Butt".len()).is_empty());
    }

    #[test]
    fn indented_uppercase_word_offers_widget_names_in_the_union() {
        // An indented CamelCase word is a child-widget position. The slot now offers the union, so
        // widget names are present (the client filters them in as the user types `Button`); a
        // catalog property is present too but the client's case-based filter drops it for `Button`.
        let styles = styles(&[("lib.otui", "Button < UIButton\nPanel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "Panel\n  Button\n";
        let items =
            complete_at_with_widgets(src, at(src, "  Button") + "  Button".len(), &styles, &lua);
        let button = items
            .iter()
            .find(|i| i.label == "Button")
            .expect("Button offered");
        assert_eq!(button.kind, CompletionKind::Class);
        assert_eq!(button.detail.as_deref(), Some("style"));
        // A lowercase-initial word at the same indentation still surfaces the catalog properties.
        let src = "Panel\n  wid\n";
        let got = labels(&complete_at_with_widgets(
            src,
            at(src, "wid") + "wid".len(),
            &styles,
            &lua,
        ));
        assert!(got.contains(&"width".to_owned()));
    }

    #[test]
    fn odd_indented_property_word_offers_nothing() {
        // A 3-space (odd) indent is a hard OTML error (`odd-indentation`); the key context must
        // honor the same indentation rule the diagnostics pass enforces → offer nothing.
        let src = "Panel\n   wid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn invalid_depth_jump_property_word_offers_nothing() {
        // `    wid` jumps from depth 0 (`Panel`) straight to depth 2 — an `invalid-indentation-depth`
        // error — so no property key may be offered on it.
        let src = "Panel\n    wid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn valid_plus_one_level_offers_the_catalog() {
        // A correctly-indented +1 level under a nested widget is valid depth: offer the catalog.
        let src = "Panel\n  Child\n    wid\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "wid") + "wid".len())),
            catalog::PROPERTIES
        );
    }

    #[test]
    fn tab_indented_property_word_offers_nothing() {
        // Tab indentation is a hard OTML parse error; no set may be offered on such a line — the key
        // context honors the same guard as the closed sets.
        let src = "Button\n\twid\n";
        assert!(complete_at(src, at(src, "wid") + "wid".len()).is_empty());
    }

    #[test]
    fn dotted_key_is_not_a_property_key() {
        // A generic dotted key (`foo.bar`) is neither a property nor an `anchors.` object → nothing.
        let src = "Button\n  foo.bar\n";
        assert!(complete_at(src, at(src, "foo.bar") + "foo.bar".len()).is_empty());
    }

    #[test]
    fn list_item_line_is_not_a_property_key() {
        // A `- item` list line opens with `-`, which is not an IDENT start → nothing.
        let src = "Button\n  - item\n";
        assert!(complete_at(src, at(src, "- item") + "- item".len()).is_empty());
        // ...even mid-word right after the dash.
        assert!(complete_at(src, at(src, "- item") + "- ".len()).is_empty());
    }

    #[test]
    fn special_form_keys_still_offer_their_own_sets_not_properties() {
        // The property-key branch runs AFTER the special forms, so `$` / `@` / `anchors.` keep
        // offering their own closed sets and never fall through to PROPERTIES.
        let src = "Button\n  $\n";
        assert_eq!(labels(&complete_at(src, at(src, "$") + 1)), schema::STATES);

        let src = "Button\n  @onC\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "@onC") + "@onC".len())),
            schema::EVENTS
        );

        let src = "Widget\n  anchors.\n";
        let mut edges: Vec<&str> = schema::ANCHOR_EDGES.to_vec();
        edges.extend_from_slice(schema::SHORTHAND_ANCHORS);
        assert_eq!(
            labels(&complete_at(src, at(src, "anchors.") + "anchors.".len())),
            edges
        );
    }

    #[test]
    fn property_key_suppressed_inside_block_scalar_and_comment() {
        // A bare word inside a `|` block body is raw Lua, and one inside a full-line comment is
        // prose — neither is a property key. The CST-suppression guard covers the key branch too.
        let src = "Button\n  @onClick: |\n    width\n";
        assert!(complete_at(src, at(src, "width\n") + "width".len()).is_empty());

        let src = "Button\n  // width\n";
        assert!(complete_at(src, at(src, "width\n") + "width".len()).is_empty());
    }

    #[test]
    fn states_context_is_deterministic() {
        // The returned labels for one context equal exactly the schema set, in const order.
        let src = "Button\n  $\n";
        assert_eq!(labels(&complete_at(src, at(src, "$") + 1)), schema::STATES);
    }

    // --- widget-aware property completion (Lua-added properties) -------------------------------

    use crate::lua_widgets::scan_widgets;
    use crate::style_index::extract_style_defs;

    fn styles(docs: &[(&str, &str)]) -> StyleIndex {
        let mut index = StyleIndex::new();
        for (doc, src) in docs {
            let tree = SyntaxTree::parse(src).expect("parse otui");
            index.set_document(*doc, extract_style_defs(&tree));
        }
        index
    }

    fn lua(docs: &[(&str, &str)]) -> LuaWidgetIndex {
        let mut index = LuaWidgetIndex::new();
        for (doc, src) in docs {
            index.set_document(*doc, scan_widgets(src));
        }
        index
    }

    const UITABLE_LUA: &str = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    elseif name == 'row-style' then
    end
  end
end
";

    #[test]
    fn slot_offers_lua_props_ranked_above_the_catalog() {
        // Under a `< UITable` header, the slot offers UITable's Lua-added properties alongside the
        // catalog, tagged and ranked above it (sort_text group `0_` < `1_`).
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Table < UITable\n  col\n";
        let items = complete_at_with_widgets(src, at(src, "col") + "col".len(), &styles, &lua);

        let col = items
            .iter()
            .find(|i| i.label == "column-style")
            .expect("lua prop offered");
        assert_eq!(col.detail.as_deref(), Some("widget property"));
        assert_eq!(col.kind, CompletionKind::Keyword);
        assert_eq!(col.sort_text.as_deref(), Some("0_column-style"));
        assert!(items.iter().any(|i| i.label == "row-style"));

        // A catalog property is present and ranked below the Lua props.
        let width = items
            .iter()
            .find(|i| i.label == "width")
            .expect("catalog prop offered");
        assert_eq!(width.sort_text.as_deref(), Some("1_width"));
        assert!(col.sort_text < width.sort_text);
    }

    #[test]
    fn slot_lua_props_resolve_cross_file_on_a_widget_instance() {
        // The cursor is under a nested `Table` container whose type resolves cross-file
        // (`Table < UITable`) to the native UITable that declares the Lua props.
        let styles = styles(&[("lib.otui", "Table < UITable\n")]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "\
Window < UIWindow
  Table
    col
";
        let got = labels(&complete_at_with_widgets(
            src,
            at(src, "col") + "col".len(),
            &styles,
            &lua,
        ));
        assert!(got.contains(&"column-style".to_owned()));
        assert!(got.contains(&"row-style".to_owned()));
    }

    #[test]
    fn slot_omits_lua_props_of_an_unrelated_widget() {
        // A Button does not descend from UITable, so UITable's Lua props are not offered on it (the
        // catalog still is; UITable may appear only as a child-widget *name*, never as a property).
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Button < UIButton\n  col\n";
        let items = complete_at_with_widgets(src, at(src, "col") + "col".len(), &styles, &lua);
        assert!(
            !items.iter().any(
                |i| i.label == "column-style" && i.detail.as_deref() == Some("widget property")
            )
        );
        assert!(items.iter().any(|i| i.label == "width"));
    }

    #[test]
    fn slot_offers_widget_names_from_every_source() {
        // Child-widget names come from workspace `.otui` styles, native bases in use, and Lua widget
        // classes — each tagged with its origin and kind `Class`.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Panel\n  Pa\n";
        let items = complete_at_with_widgets(src, at(src, "Pa\n") + "Pa".len(), &styles, &lua);

        let named = |label: &str| items.iter().find(|i| i.label == label).cloned();
        let panel = named("Panel").expect("user style offered");
        assert_eq!(panel.kind, CompletionKind::Class);
        assert_eq!(panel.detail.as_deref(), Some("style"));
        assert_eq!(panel.sort_text.as_deref(), Some("2_Panel"));

        assert_eq!(
            named("UIWidget").and_then(|i| i.detail),
            Some("native widget".to_owned())
        );
        assert_eq!(
            named("UITable").and_then(|i| i.detail),
            Some("lua widget".to_owned())
        );
    }

    #[test]
    fn native_widget_classes_surface_from_lua_parents_without_a_style_reference() {
        // No `.otui` style references `UIWidget`, but the scanned Lua widget `UITable` extends it, so
        // the native base is still offered as a child type — the built-in `UI*` classes come from the
        // corelib/gamelib widgets' parents, not a hardcoded list.
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]); // UITable = extends(UIWidget, 'UITable')
        let src = "SomeWidget\n  UI\n";
        let items = complete_at_with_widgets(src, at(src, "UI\n") + "UI".len(), &styles, &lua);
        let ui_widget = items.iter().find(|i| i.label == "UIWidget").cloned();
        assert_eq!(
            ui_widget.and_then(|i| i.detail),
            Some("native widget".to_owned()),
            "UIWidget must be offered as a native child even with no style referencing it"
        );
    }

    #[test]
    fn empty_slot_before_existing_content_offers_the_union() {
        // The cursor sits at the start of an indented line that already has a word after it — an
        // empty slot the classifier now opens (the line is not wholly blank, so the indentation gate
        // passes). The union (catalog + widget names) is offered.
        let styles = styles(&[("lib.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);
        let src = "Panel\n  Button\n";
        let items = complete_at_with_widgets(src, at(src, "Button"), &styles, &lua);
        let got = labels(&items);
        assert!(got.contains(&"width".to_owned()), "catalog present");
        assert!(got.contains(&"Button".to_owned()), "widget names present");
    }

    #[test]
    fn empty_slot_before_a_special_form_offers_nothing() {
        // The cursor sits at the start of a line whose real content is a `$state` / `@event` /
        // `anchors.` token (the empty prefix cannot see it). The property/widget union must NOT fire
        // there — offer nothing rather than the wrong set.
        let styles = styles(&[("lib.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);
        for line in [
            "$hover",
            "@onClick: x",
            "anchors.top: parent",
            "&alias: 1",
            "- item",
        ] {
            let src = format!("Button\n  {line}\n");
            let offset = src.find(line).expect("token present"); // cursor before the token
            assert!(
                complete_at_with_widgets(&src, offset, &styles, &lua).is_empty(),
                "empty slot before `{line}` must offer nothing"
            );
        }
    }

    #[test]
    fn empty_indexes_degrade_to_the_catalog_only() {
        // The workspace-unaware `complete_at` (empty indexes) offers exactly the catalog on a key —
        // no Lua props and no widget names, so the labels are the catalog in const order.
        let src = "Table < UITable\n  col\n";
        assert_eq!(
            labels(&complete_at(src, at(src, "col") + "col".len())),
            catalog::PROPERTIES
        );
    }

    // --- snippets --------------------------------------------------------------------------------

    #[test]
    fn catalog_property_key_snippet_is_key_colon_dollar_zero() {
        // `width` → `width: $0`: the colon-space and a final tab-stop for the value, saving a
        // keystroke on every property.
        let src = "Button\n  wid\n";
        let items = complete_at(src, at(src, "wid") + "wid".len());
        let width = items.iter().find(|i| i.label == "width").expect("width");
        assert_eq!(width.insert_text.as_deref(), Some("width: $0"));
        assert_eq!(width.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn lua_property_key_also_carries_the_key_colon_snippet() {
        let styles = styles(&[]);
        let lua = lua(&[("uitable.lua", UITABLE_LUA)]);
        let src = "Table < UITable\n  col\n";
        let items = complete_at_with_widgets(src, at(src, "col") + "col".len(), &styles, &lua);
        let col = items
            .iter()
            .find(|i| i.label == "column-style")
            .expect("lua prop");
        assert_eq!(col.insert_text.as_deref(), Some("column-style: $0"));
        assert_eq!(col.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn child_widget_snippet_carries_a_nested_id_tab_stop_one_level_deeper() {
        // Cursor at 2-space indent (top-level child slot): the snippet's `id:` line sits at 4 spaces
        // — one level deeper than the widget itself.
        let styles = styles(&[("lib.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);
        let src = "Panel\n  But\n";
        let items = complete_at_with_widgets(src, at(src, "But") + "But".len(), &styles, &lua);
        let button = items.iter().find(|i| i.label == "Button").expect("Button");
        assert_eq!(
            button.insert_text.as_deref(),
            Some("Button\n    id: $1\n    $0")
        );
        assert_eq!(button.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn child_widget_snippet_nests_relative_to_a_deeper_current_indent() {
        // A child slot at 4-space indent (already one level deep): the nested `id:` line sits at 6
        // spaces, still exactly one level deeper than the current slot, not a fixed absolute amount.
        let styles = styles(&[("lib.otui", "Button < UIButton\n")]);
        let lua = lua(&[]);
        let src = "Panel\n  Child\n    But\n";
        let items = complete_at_with_widgets(src, at(src, "But") + "But".len(), &styles, &lua);
        let button = items.iter().find(|i| i.label == "Button").expect("Button");
        assert_eq!(
            button.insert_text.as_deref(),
            Some("Button\n      id: $1\n      $0")
        );
    }

    #[test]
    fn event_key_snippet_continues_into_the_value_slot() {
        let src = "Button\n  @onCl\n";
        let items = complete_at(src, at(src, "@onCl") + "@onCl".len());
        let on_click = items
            .iter()
            .find(|i| i.label == "onClick")
            .expect("onClick");
        assert_eq!(on_click.insert_text.as_deref(), Some("onClick: $0"));
        assert_eq!(on_click.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn anchor_edge_key_snippet_continues_into_the_target_slot() {
        let src = "Widget\n  anchors.\n";
        let items = complete_at(src, at(src, "anchors.") + "anchors.".len());
        let top = items.iter().find(|i| i.label == "top").expect("top edge");
        assert_eq!(top.insert_text.as_deref(), Some("top: $0"));
        assert_eq!(top.insert_format, InsertFormat::Snippet);
    }

    #[test]
    fn anchor_shorthand_key_also_carries_the_key_colon_snippet() {
        let src = "Widget\n  anchors.\n";
        let items = complete_at(src, at(src, "anchors.") + "anchors.".len());
        for &shorthand in schema::SHORTHAND_ANCHORS {
            let item = items
                .iter()
                .find(|i| i.label == shorthand)
                .unwrap_or_else(|| panic!("{shorthand} offered"));
            assert_eq!(
                item.insert_text.as_deref(),
                Some(format!("{shorthand}: $0")).as_deref()
            );
        }
    }

    #[test]
    fn anchor_edge_and_shorthand_completions_carry_documentation() {
        // Anchor edges/shorthands are not catalog properties, but they get their own documentation
        // from the shared `property_hover::documentation_body` formatter, stating BOTH that an
        // invalid edge is rejected (spec §2.4, INVALID_ANCHOR_EDGE) and the precise resolution scope:
        // a direct sibling id or a magic pseudo-target only.
        let src = "Widget\n  anchors.\n";
        let items = complete_at(src, at(src, "anchors.") + "anchors.".len());
        let top = items.iter().find(|i| i.label == "top").expect("top edge");
        let top_doc = top.documentation.as_deref().expect("top has a doc");
        assert!(top_doc.contains("edge"), "{top_doc}");
        assert!(top_doc.contains("rejects an invalid"), "{top_doc}");
        assert!(top_doc.contains("direct sibling"), "{top_doc}");

        let fill = items.iter().find(|i| i.label == "fill").expect("fill");
        let fill_doc = fill.documentation.as_deref().expect("fill has a doc");
        assert!(
            fill_doc.to_lowercase().contains("all four edges"),
            "{fill_doc}"
        );
        assert!(fill_doc.contains("rejects an invalid"), "{fill_doc}");
        assert!(fill_doc.contains("direct sibling"), "{fill_doc}");

        // An `@event` name gets no documentation — no curated note exists for those.
        let src = "Button\n  @onCl\n";
        let items = complete_at(src, at(src, "@onCl") + "@onCl".len());
        let on_click = items
            .iter()
            .find(|i| i.label == "onClick")
            .expect("onClick");
        assert_eq!(on_click.documentation, None);
    }

    #[test]
    fn anchor_target_edge_and_states_and_property_values_stay_plain() {
        // The target-side edge (`parent.top`), a `$state`, and a fixed-set value (`display: flex`)
        // have no natural "what comes next" — they stay plain-label insertion (no snippet).
        let src = "Widget\n  anchors.top: parent.\n";
        let items = complete_at(src, at(src, "parent.") + "parent.".len());
        assert!(
            items
                .iter()
                .all(|i| i.insert_text.is_none() && i.insert_format == InsertFormat::Plain)
        );

        let src = "Button\n  $\n";
        let items = complete_at(src, at(src, "$") + 1);
        assert!(
            items
                .iter()
                .all(|i| i.insert_text.is_none() && i.insert_format == InsertFormat::Plain)
        );

        let src = "Widget\n  display: fl\n";
        let items = complete_at(src, at(src, "fl") + 2);
        assert!(
            items
                .iter()
                .all(|i| i.insert_text.is_none() && i.insert_format == InsertFormat::Plain)
        );

        // Anchor targets (magic keywords / widget ids) are also plain.
        let src = "Widget\n  anchors.top: \n";
        let items = complete_at(src, at(src, "anchors.top: ") + "anchors.top: ".len());
        assert!(
            items
                .iter()
                .all(|i| i.insert_text.is_none() && i.insert_format == InsertFormat::Plain)
        );
    }

    #[test]
    fn snippet_escape_protects_dollar_backslash_and_closing_brace() {
        // The direct unit: a literal `$`, `\` or `}` in text embedded into a snippet body must come
        // back escaped, or the client would read it as the client's own tab-stop syntax instead of
        // literal text. Not a theoretical hazard: OTML property values and identifiers routinely
        // start with `$` (`$state`, `$variable`).
        assert_eq!(snippet_escape("plain"), "plain");
        assert_eq!(snippet_escape("$state"), "\\$state");
        assert_eq!(snippet_escape("a}b"), "a\\}b");
        assert_eq!(snippet_escape("a\\b"), "a\\\\b");
        assert_eq!(snippet_escape("$a}b\\c"), "\\$a\\}b\\\\c");
    }

    #[test]
    fn property_key_snippet_escapes_a_dollar_bearing_key() {
        // A defensive, end-to-end check through the actual snippet builder (not just the escape
        // helper): a hypothetical key containing `$` must not corrupt the emitted snippet's tab-stop
        // syntax.
        assert_eq!(property_key_snippet("$weird"), "\\$weird: $0");
    }

    #[test]
    fn child_widget_snippet_escapes_a_dollar_bearing_widget_name() {
        assert_eq!(
            child_widget_snippet("$Weird", "  "),
            "\\$Weird\n  id: $1\n  $0"
        );
    }

    #[test]
    fn a_lua_property_named_with_a_dollar_sign_is_escaped_in_its_snippet() {
        // End-to-end through the workspace-aware path: a Lua-scanned custom property name flowing
        // into the snippet body is escaped, not just the schema/catalog constants.
        const WEIRD_LUA: &str = "\
Weird = extends(UIWidget, 'Weird')

function Weird:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == '$odd-style' then
    end
  end
end
";
        let styles = styles(&[]);
        let lua = lua(&[("weird.lua", WEIRD_LUA)]);
        let src = "Table < Weird\n  od\n";
        let items = complete_at_with_widgets(src, at(src, "od") + "od".len(), &styles, &lua);
        let odd = items
            .iter()
            .find(|i| i.label == "$odd-style")
            .expect("lua prop offered even with a $ in its name");
        assert_eq!(odd.insert_text.as_deref(), Some("\\$odd-style: $0"));
    }

    // --- `Name < Base` style-header base slot (spec §6, §2.2) -------------------------------------

    #[test]
    fn base_slot_offers_workspace_styles_and_native_bases() {
        // Cursor right after `<` on a top-level header: offer the valid-base union — every
        // workspace style name plus the `UI*` native classes in use.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "Table < \n";
        let items = complete_at_with_widgets(src, at(src, "< ") + "< ".len(), &styles, &lua);
        let got = labels(&items);
        assert!(got.contains(&"Panel".to_owned()), "workspace style offered");
        assert!(got.contains(&"UIWidget".to_owned()), "native base offered");
        assert!(items.iter().all(|i| i.kind == CompletionKind::Class));
        // A base is a plain type reference — no `id:` child snippet, unlike a child-widget entry.
        assert!(items.iter().all(|i| i.insert_text.is_none()));
        assert!(items.iter().all(|i| i.insert_format == InsertFormat::Plain));
    }

    #[test]
    fn base_slot_partial_token_still_offers_the_whole_union() {
        // A partial base name being typed still offers the whole set; the client filters.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "Foo < UIB\n";
        let items = complete_at_with_widgets(src, at(src, "UIB") + "UIB".len(), &styles, &lua);
        let got = labels(&items);
        assert!(got.contains(&"Panel".to_owned()));
        assert!(got.contains(&"UIWidget".to_owned()));
    }

    #[test]
    fn frozen_header_base_slot_offers_nothing() {
        // `#Name < Base` — a leading `#` makes the WHOLE line a comment (otmlparser.cpp:311), so
        // the base slot on a frozen header offers nothing.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "#Table < \n";
        let items = complete_at_with_widgets(src, at(src, "< ") + "< ".len(), &styles, &lua);
        assert!(items.is_empty());
    }

    #[test]
    fn slashslash_comment_containing_an_angle_bracket_offers_nothing() {
        // A top-level `//` comment that happens to contain a `<` must not be mistaken for a header
        // base slot: the CST suppresses completion inside a comment before `classify` ever runs.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "// a < b\n";
        let items = complete_at_with_widgets(src, at(src, "< ") + "< ".len(), &styles, &lua);
        assert!(items.is_empty());
    }

    #[test]
    fn base_slot_name_side_offers_nothing() {
        // Before the `<`, the cursor sits on the header's NAME side — the author's own new name,
        // not a reference — so nothing is offered there.
        let src = "Tab\n";
        assert!(complete_at(src, at(src, "Tab") + "Tab".len()).is_empty());
    }

    #[test]
    fn base_slot_whitespace_variants_after_the_angle_bracket_both_offer_the_union() {
        // `Name <|` (no space yet) and `Name < |` (a space already typed) both offer the union —
        // the segment's leading whitespace is stripped either way.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);

        let src = "Table <\n";
        let items = complete_at_with_widgets(src, at(src, "<") + 1, &styles, &lua);
        assert!(labels(&items).contains(&"Panel".to_owned()));

        let src2 = "Table < \n";
        let items2 = complete_at_with_widgets(src2, at(src2, "< ") + 2, &styles, &lua);
        assert!(labels(&items2).contains(&"Panel".to_owned()));
    }

    #[test]
    fn base_slot_completed_token_followed_by_more_content_offers_nothing() {
        // `style_base` (grammar.js:111) is one greedy whole-line token: once the segment after `<`
        // holds a completed word followed by more content (interior whitespace here), it is no
        // longer being built.
        let src = "Table < UIWidget extra\n";
        let offset = at(src, "extra") + "extra".len();
        assert!(complete_at(src, offset).is_empty());
    }

    #[test]
    fn indented_line_is_unaffected_by_the_base_slot_branch() {
        // Regression: an indented line (the ordinary IndentedSlot union) must not be routed into
        // the new base-slot branch, even though the header line above it contains a `<`.
        let styles = styles(&[("lib.otui", "Panel < UIWidget\n")]);
        let lua = lua(&[]);
        let src = "Table < UIWidget\n  wid\n";
        let items = complete_at_with_widgets(src, at(src, "wid") + "wid".len(), &styles, &lua);
        assert!(
            labels(&items).contains(&"width".to_owned()),
            "catalog present"
        );
    }
}
