//! Scanning OTClient **Lua** modules for the custom style properties widgets add at runtime.
//!
//! Some OTClient widgets are C++ engine classes; others (and some engine ones) read *extra* style
//! attributes in Lua that the C++ property catalog knows nothing about. `UITable`, for example, is
//! declared in `modules/corelib/ui/uitable.lua` and consumes `column-style`, `row-style`,
//! `header-column-style`, `header-row-style` and `table-data` — none of which are engine property
//! tags. A `.otui` using them on a `UITable` is perfectly valid, but a catalog-only
//! `unknown-property` check has no way to know that and would wrongly flag them.
//!
//! This module closes that gap by scanning Lua **text** and recording, per widget, the custom style
//! properties it declares and the widget it inherits from. Like [`style_index`](crate::style_index)
//! it is **pure**: it is handed file contents (`&str`), returns byte/`String` data, and touches no
//! filesystem and no `lsp-types`. The server owns the I/O — walking `modules/**/ui/*.lua`, feeding
//! each file here, and aggregating the results in a [`LuaWidgetIndex`] keyed by document.
//!
//! ## What is recognized (the mechanism)
//!
//! A widget's own class and its parent come from its class-declaration line, and OTClient spells
//! that as `extends`:
//!
//! ```lua
//! UITable = extends(UIWidget, 'UITable')
//! ```
//!
//! The **first** `extends` argument is the parent class ([`LuaWidgetDef::lua_parent`]); the assigned
//! global on the left (`UITable`) is the widget name we key by — in practice identical to the
//! registered string name in the second argument.
//!
//! A widget's custom style properties are the string literals it compares the style-key against
//! inside its `onStyleApply` method:
//!
//! ```lua
//! function UITable:onStyleApply(styleName, styleNode)
//!   for name, value in pairs(styleNode) do
//!     if name == 'table-data' then ...
//!     elseif name == 'column-style' then ...
//!     end
//!   end
//! end
//! ```
//!
//! Two read forms inside the `onStyleApply` body are recognized (both are how a widget pulls a
//! custom attribute out of the applied style):
//!
//! * the **equality chain** — `<key> == '<prop>'` (or `"<prop>"`, or the reversed
//!   `'<prop>' == <key>`) over the `for <key>, … in pairs(styleNode)` loop variable; and
//! * **direct style-node reads** — a `styleNode.<field>` field access (a non-hyphenated key, e.g.
//!   `styleNode.options` on `UIComboBox`) or a `styleNode['<key>']` / `styleNode["<key>"]` index
//!   access (which may be hyphenated, e.g. `styleNode['tab-spacing']` on `UIMoveableTabBar`). The
//!   style-node parameter name is read from the `onStyleApply` signature, defaulting to `styleNode`.
//!
//! A module may also attach a style handler as a **signal** rather than overriding the method, which
//! is how a property becomes valid on a class the module does not own:
//!
//! ```lua
//! local function onWidgetStyleApply(widget, styleName, styleNode)
//!   if styleNode.tooltip then widget.tooltip = styleNode.tooltip end
//! end
//! connect(UIWidget, { onStyleApply = onWidgetStyleApply })
//! ```
//!
//! The handler is a plain function, so the method pass never sees it. `connect(<Class>, {onStyleApply
//! = <fn>})` is therefore resolved to its handler, whose body is scanned with the same two read forms
//! — noting that a signal handler takes the widget as its first argument, so its style node is the
//! **third** parameter where a method's is the second. Connecting to `UIWidget` (as the tooltip module
//! does) makes the properties valid on every widget, since `UIWidget` is every widget's implicit root
//! in [`widget_resolve`](crate::widget_resolve).
//!
//! ## Deliberately not (yet) covered — the fidelity gap
//!
//! `onStyleApply` is the dominant but not the only place a widget reads custom attributes. These are
//! **not** recognized here, and are left for later widening as real false positives surface:
//!
//! * custom attributes applied via `mergeStyle`/`applyStyle` elsewhere in the class;
//! * a style-node read routed through another local (the table aliased to a different variable).
//!
//! C++-side `onStyleApply` overrides are covered, but not here: they are machine-extracted from the
//! engine source into `catalog::NATIVE_WIDGET_PROPERTIES` (see `schema::native_widget_declares`),
//! since this module only ever sees Lua.
//!
//! Keeping the covered set explicit means the gap is visible rather than silently assumed complete.
//!
//! ## Heuristic parse (no Lua grammar)
//!
//! There is no Lua parser in this workspace, so the scan is line/byte oriented and intentionally
//! conservative:
//!
//! * an `extends(...)` declaration is read from a `<ident> = extends(<parent>, …)` line;
//! * an `onStyleApply` body runs from its `function <W>:onStyleApply(` line up to (but not
//!   including) the next column-0 `function ` / `end` line, or end of file — every method here is a
//!   top-level (column-0) function, so this bounds the body reliably;
//! * `extends`/comparisons appearing inside strings or comments are not specially excluded — an
//!   acceptable trade for staying dependency-free. Overreach only ever *adds* a known property,
//!   which softens a hint; it never invents an error.

use crate::style_index::DocId;
use std::collections::{BTreeSet, HashMap};

/// One widget's Lua-declared style contract, extracted from a single Lua source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaWidgetDef {
    /// The widget name — the global assigned by `<name> = extends(...)` and/or the receiver of a
    /// `function <name>:onStyleApply(...)` method. This is the name a `.otui` `< Base` header
    /// resolves to.
    pub name: String,
    /// The parent class from the widget's `extends(<parent>, …)` declaration, if one was found.
    /// `None` when the widget is only seen through its `onStyleApply` method (no `extends` line in
    /// the scanned text), or its declaration is malformed.
    pub lua_parent: Option<String>,
    /// The custom style property keys the widget's `onStyleApply` compares against. Empty for a
    /// widget known only through its `extends` line. Sorted and de-duplicated.
    pub custom_props: BTreeSet<String>,
}

/// Extract every widget declared in one Lua `source`.
///
/// Combines the two mechanisms — `extends(...)` for parents and `onStyleApply` for custom props —
/// into one [`LuaWidgetDef`] per widget name. A widget is emitted if it appears in **either** pass:
/// an `extends` line with no `onStyleApply` yields an empty `custom_props`; an `onStyleApply` with
/// no visible `extends` yields `lua_parent: None`. The returned list is ordered by widget name
/// (deterministic across runs). A source with neither construct yields an empty vector.
#[must_use]
pub fn scan_widgets(source: &str) -> Vec<LuaWidgetDef> {
    // name -> (parent, props); a BTreeMap keeps the output deterministically name-ordered.
    let mut widgets: std::collections::BTreeMap<String, (Option<String>, BTreeSet<String>)> =
        std::collections::BTreeMap::new();

    // Pass 1: `<name> = extends(<parent>, …)` declarations.
    for line in source.lines() {
        if let Some((name, parent)) = parse_extends(line) {
            let entry = widgets.entry(name).or_default();
            // Keep the first parent seen; a later re-declaration does not overwrite it.
            if entry.0.is_none() {
                entry.0 = parent;
            }
        }
    }

    // Pass 2: `function <name>:onStyleApply(...)` bodies and their `<key> == '<prop>'` comparisons.
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(widget) = parse_on_style_apply_header(lines[i]) {
            let body_end = on_style_apply_body_end(&lines, i + 1);
            let props = collect_props(&lines[i..body_end]);
            let entry = widgets.entry(widget).or_default();
            entry.1.extend(props);
            i = body_end;
        } else {
            i += 1;
        }
    }

    // Pass 3: `connect(<Class>, { onStyleApply = <fn> })` signal hooks. The handler is a plain
    // function, not a method, so pass 2 never sees it — yet it reads style attributes just the same,
    // and connecting it to `UIWidget` makes them valid on *every* widget (that is how `tooltip:`
    // works). Resolve each hook's handler by name and scan its body.
    for (class, handler) in parse_connect_style_hooks(&lines) {
        let Some(body) = handler_body(&lines, &handler) else {
            continue;
        };
        let props = collect_handler_props(body);
        if props.is_empty() {
            continue;
        }
        widgets.entry(class).or_default().1.extend(props);
    }

    widgets
        .into_iter()
        .map(|(name, (lua_parent, custom_props))| LuaWidgetDef {
            name,
            lua_parent,
            custom_props,
        })
        .collect()
}

/// Find every `connect(<Class>, { … onStyleApply = <handler> … })` in `lines`, returning
/// `(class, handler-name)` pairs.
///
/// A module can hook a *signal* handler onto a widget class instead of overriding the method — the
/// engine's tooltip module does exactly this:
///
/// ```lua
/// connect(UIWidget, { onStyleApply = onWidgetStyleApply, onHoverChange = … })
/// ```
///
/// The handler then reads `styleNode.tooltip`, which makes `tooltip:` a valid style property on the
/// connected class and everything descending from it.
///
/// The table literal may span lines, so the scan continues past the `connect(` — but it stops at the
/// **end of that table** (a line closing it with `}`) or at the next `connect(`, whichever comes
/// first. Scanning blindly ahead would let a *later* `connect(OtherClass, {onStyleApply = …})` donate
/// its handler to a preceding `connect(` that declared no style hook of its own, attributing the
/// properties to the wrong class.
fn parse_connect_style_hooks(lines: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = find_call_arg(line, "connect") else {
            continue;
        };
        // The connected class is the first argument.
        let Some(class) = rest
            .split(',')
            .next()
            .and_then(|a| last_identifier(a.trim()))
        else {
            continue;
        };
        for (offset, hook_line) in lines[i..].iter().enumerate() {
            // Stop at the next `connect(` — its table is not ours.
            if offset > 0 && find_call_arg(hook_line, "connect").is_some() {
                break;
            }
            if let Some(handler) = table_entry_value(hook_line, "onStyleApply") {
                out.push((class.clone(), handler));
                break;
            }
            // Stop at the end of this table. Checked *after* the entry test so a single-line
            // `connect(C, { onStyleApply = h })` still resolves.
            if offset > 0 && hook_line.trim_start().starts_with('}') {
                break;
            }
        }
    }
    out
}

/// The text after `<name>(` on `line`, or `None` when the call does not appear as a whole word.
fn find_call_arg<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let idx = line.find(&format!("{name}("))?;
    // `map_or(true, …)` rather than `Option::is_none_or` — the latter is only stable since Rust
    // 1.82, but the workspace MSRV is 1.75.
    let before_ok = line[..idx]
        .chars()
        .last()
        .map_or(true, |c| !c.is_alphanumeric() && c != '_');
    before_ok.then(|| &line[idx + name.len() + 1..])
}

/// The value of a `<key> = <value>` entry in a Lua table literal, e.g. the `onWidgetStyleApply` in
/// `onStyleApply = onWidgetStyleApply,`. `None` when the line carries no such entry.
///
/// Found anywhere on the line, not just at its start, so a single-line table
/// (`connect(C, { onStyleApply = h })`) resolves as well as the multi-line form. Matched as a whole
/// word (a `myOnStyleApply = f` is not it) and `==` is rejected — that is a comparison, not an entry.
fn table_entry_value(line: &str, key: &str) -> Option<String> {
    let mut search = 0;
    while let Some(rel) = line[search..].find(key) {
        let idx = search + rel;
        search = idx + key.len();

        // Whole-word: `myOnStyleApply = f` is not the `onStyleApply` entry.
        // `map_or(true, …)` rather than `Option::is_none_or` — the latter is only stable since Rust
        // 1.82, but the workspace MSRV is 1.75.
        let word_start = line[..idx]
            .chars()
            .last()
            .map_or(true, |c| !c.is_alphanumeric() && c != '_');
        if !word_start {
            continue;
        }
        let Some(rest) = line[search..].trim_start().strip_prefix('=') else {
            continue;
        };
        // Reject `==` (a comparison, not a table entry).
        if rest.starts_with('=') {
            continue;
        }
        // The value runs to the entry's end: the next `,`, or the table's `}`.
        let value = rest.trim_start();
        let end = value.find([',', '}', ')']).unwrap_or(value.len());
        if let Some(id) = last_identifier(value[..end].trim()) {
            return Some(id);
        }
    }
    None
}

/// The body of the top-level function named `name` — `[local ]function <name>(` up to (but not
/// including) the next column-0 `end`. Returns the body **including** its header line, so the
/// argument list can be read from it. `None` when no such function is defined in `lines`.
fn handler_body<'a>(lines: &'a [&'a str], name: &str) -> Option<&'a [&'a str]> {
    let start = lines.iter().position(|line| {
        let t = line.trim_start();
        let t = t.strip_prefix("local ").unwrap_or(t);
        t.strip_prefix("function ")
            .and_then(|r| find_call_arg(r, name).map(|_| ()))
            .is_some()
            || t.strip_prefix("function ")
                .is_some_and(|r| r.trim_start().starts_with(&format!("{name}(")))
    })?;
    let mut end = start + 1;
    while end < lines.len() && !starts_with_end_keyword(lines[end]) {
        end += 1;
    }
    Some(&lines[start..end])
}

/// The custom style properties a **connected signal handler** reads, like [`collect_props`] but for a
/// handler signature rather than a method's.
///
/// A signal handler receives the widget as its first argument — `function h(widget, styleName,
/// styleNode)` — so the style node is the **third** parameter, where a method's `onStyleApply` (whose
/// `self` is implicit) has it second. Both read forms are otherwise identical, so the same
/// `styleNode.<field>` / `styleNode['<key>']` / `<key> == '<prop>'` scanners apply.
fn collect_handler_props(body: &[&str]) -> BTreeSet<String> {
    let node_var = body
        .first()
        .and_then(|header| {
            let open = header.find('(')?;
            let close_rel = header[open..].find(')')?;
            header[open + 1..open + close_rel]
                .split(',')
                .nth(2)
                .and_then(|a| last_identifier(a.trim()))
        })
        .unwrap_or_else(|| "styleNode".to_owned());

    let key_var = style_key_variable(body, &node_var);
    let mut props = BTreeSet::new();
    for line in body {
        if let Some(prop) = comparison_literal(line, &key_var) {
            props.insert(prop);
        }
        collect_style_node_reads(line, &node_var, &mut props);
    }
    props
}

/// Parse a `<name> = extends(<parent>, …)` line, returning `(name, parent)`.
///
/// `parent` is `None` if the first `extends` argument is not a bare identifier. Returns `None` when
/// the line is not an `extends` assignment at all.
fn parse_extends(line: &str) -> Option<(String, Option<String>)> {
    let eq_at = line.find('=')?;
    // The left side must be a single identifier (the assigned global).
    let name = last_identifier(line[..eq_at].trim())?;

    let rest = line[eq_at + 1..].trim_start();
    let after_kw = rest.strip_prefix("extends")?.trim_start();
    let args = after_kw.strip_prefix('(')?;

    // First argument up to the first ',' or ')'.
    let first_end = args.find([',', ')']).unwrap_or(args.len());
    let parent = last_identifier(args[..first_end].trim());

    Some((name, parent))
}

/// Return the widget name from a `function <name>:onStyleApply(` header line, or `None`.
fn parse_on_style_apply_header(line: &str) -> Option<String> {
    let after_fn = line.trim_start().strip_prefix("function ")?;
    let colon = after_fn.find(':')?;
    let method_start = colon + 1;
    let method = &after_fn[method_start..];
    // The method must be exactly `onStyleApply` followed by its argument list.
    if !method.starts_with("onStyleApply") {
        return None;
    }
    let after_name = &method["onStyleApply".len()..];
    if !after_name.trim_start().starts_with('(') {
        return None;
    }
    last_identifier(after_fn[..colon].trim())
}

/// Find the exclusive end index of the `onStyleApply` body that starts at `lines[start]`.
///
/// The body ends at the next column-0 `function ` / `end` line (the top-level constructs that
/// necessarily close the method), or at end of file. Nested, indented `end`s are skipped because
/// only column-0 markers terminate a top-level function.
fn on_style_apply_body_end(lines: &[&str], start: usize) -> usize {
    for (offset, line) in lines[start..].iter().enumerate() {
        // A column-0 marker (no leading whitespace) closes the top-level function.
        let is_col0 = !line.starts_with([' ', '\t']);
        if is_col0 && (line.starts_with("function ") || starts_with_end_keyword(line)) {
            // Include the closing `end` line itself so its (empty) content is scanned harmlessly.
            return (start + offset + 1).min(lines.len());
        }
    }
    lines.len()
}

/// Whether `line` begins with the `end` keyword as a whole word (e.g. `end`, `end)`), not merely
/// a longer identifier that happens to start with those letters.
fn starts_with_end_keyword(line: &str) -> bool {
    let rest = match line.strip_prefix("end") {
        Some(r) => r,
        None => return false,
    };
    rest.chars().next().map_or(true, |c| !is_ident_char(c))
}

/// Collect the custom property literals from an `onStyleApply` body (the slice of lines from the
/// `function …:onStyleApply` header through its closing marker).
///
/// Two read forms are recognized: the `<key> == '<prop>'` equality chain over the loop key
/// variable, and direct reads of the style-node table — `<styleNode>.<field>` and
/// `<styleNode>['<key>']`. Both are how a widget pulls a custom attribute out of the applied style.
fn collect_props(body: &[&str]) -> BTreeSet<String> {
    let node_var = body.first().map_or_else(
        || "styleNode".to_owned(),
        |header| style_node_variable(header),
    );
    let key_var = style_key_variable(body, &node_var);
    let mut props = BTreeSet::new();
    for line in body {
        if let Some(prop) = comparison_literal(line, &key_var) {
            props.insert(prop);
        }
        collect_style_node_reads(line, &node_var, &mut props);
    }
    props
}

/// The style-node table parameter of an `onStyleApply` header, i.e. the `styleNode` in
/// `function W:onStyleApply(styleName, styleNode)` — the second argument. Falls back to the
/// conventional `styleNode` when the argument list cannot be read, so a direct read still resolves.
fn style_node_variable(header: &str) -> String {
    let default = || "styleNode".to_owned();
    let Some(open) = header.find('(') else {
        return default();
    };
    let Some(close_rel) = header[open..].find(')') else {
        return default();
    };
    let args = &header[open + 1..open + close_rel];
    // The style node is the second parameter (after the style name).
    match args
        .split(',')
        .nth(1)
        .and_then(|a| last_identifier(a.trim()))
    {
        Some(var) => var,
        None => default(),
    }
}

/// Collect every direct style-node read on `line` into `props`: a `<var>.<field>` field access
/// contributes `<field>` (a non-hyphenated key), and a `<var>['<key>']` / `<var>["<key>"]` index
/// access contributes the quoted key (which may be hyphenated). Only whole-word matches of `var`
/// count, so `myStyleNode` does not match `styleNode`.
fn collect_style_node_reads(line: &str, var: &str, props: &mut BTreeSet<String>) {
    let mut search = 0;
    while let Some(rel) = line[search..].find(var) {
        let start = search + rel;
        let end = start + var.len();
        search = end;
        // Whole-word match: the char before `var` must not be part of a longer identifier.
        if line[..start].chars().next_back().is_some_and(is_ident_char) {
            continue;
        }
        let rest = &line[end..];
        if let Some(after_dot) = rest.strip_prefix('.') {
            let field: String = after_dot
                .chars()
                .take_while(|&c| is_ident_char(c))
                .collect();
            if !field.is_empty() {
                props.insert(field);
            }
        } else if let Some(after_bracket) = rest.strip_prefix('[') {
            if let Some(key) = leading_string_literal(after_bracket.trim_start()) {
                props.insert(key);
            }
        }
    }
}

/// The loop key variable iterating the style node, i.e. the `k` in `for k, v in pairs(<node_var>)`,
/// where `node_var` is the style-node parameter resolved by [`style_node_variable`] (so a renamed
/// node parameter is honored here just as it is for direct reads).
///
/// Falls back to `name` (the overwhelmingly common spelling) when no such loop is found, so a body
/// that reads the key through the conventional variable still resolves.
fn style_key_variable(body: &[&str], node_var: &str) -> String {
    let pairs = format!("pairs({node_var})");
    for line in body {
        if !line.contains(&pairs) {
            continue;
        }
        if let Some(after_for) = line.trim_start().strip_prefix("for ") {
            let first = after_for
                .split([',', ' ', '\t'])
                .next()
                .unwrap_or("")
                .trim();
            if let Some(var) = last_identifier(first) {
                return var;
            }
        }
    }
    "name".to_owned()
}

/// Extract the quoted literal from a `<key_var> == '<lit>'` (or reversed `'<lit>' == <key_var>`)
/// comparison on `line`, or `None` if the line carries no such comparison.
///
/// Only the first `==` on the line is considered — the equality chains scanned here put one
/// comparison per line.
fn comparison_literal(line: &str, key_var: &str) -> Option<String> {
    let eq_at = find_double_equals(line)?;
    let left = line[..eq_at].trim_end();
    let right = line[eq_at + 2..].trim_start();

    // `<key_var> == '<lit>'`
    if ends_with_word(left, key_var) {
        if let Some(lit) = leading_string_literal(right) {
            return Some(lit);
        }
    }
    // `'<lit>' == <key_var>` — the literal is the trailing token of the left side.
    if starts_with_word(right, key_var) {
        if let Some(lit) = trailing_string_literal(left) {
            return Some(lit);
        }
    }
    None
}

/// Byte index of the first `==` that is a comparison operator (not `==` inside `===`, which Lua
/// does not have anyway, and not the tail of `~=`/`<=`/`>=`). We only need to avoid matching an `=`
/// that is really the second char of another operator; scanning for a standalone `==` suffices.
fn find_double_equals(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'=' && bytes[i + 1] == b'=' {
            // Not preceded by another comparison char (`~<>=`) that would make this a 3-char op.
            let prev_ok = i == 0 || !matches!(bytes[i - 1], b'~' | b'<' | b'>' | b'=');
            if prev_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The inner text of a single- or double-quoted string literal at the start of `s`, or `None` if
/// `s` does not begin with a quote. Reads up to the matching closing quote (no escape handling —
/// style property keys never contain quotes or backslashes).
fn leading_string_literal(s: &str) -> Option<String> {
    let mut chars = s.char_indices();
    let (_, quote) = chars.next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    for (idx, c) in chars {
        if c == quote {
            return Some(s[quote.len_utf8()..idx].to_owned());
        }
    }
    None
}

/// The inner text of a single- or double-quoted string literal at the **end** of `s`, or `None` if
/// `s` does not end with a quote. The mirror of [`leading_string_literal`] for the reversed
/// `'<lit>' == <key_var>` comparison, where the literal is the left operand's trailing token.
fn trailing_string_literal(s: &str) -> Option<String> {
    let quote = s.chars().next_back()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let inner_end = s.len() - quote.len_utf8();
    // Find the matching opening quote scanning back from just before the closing one.
    let open = s[..inner_end].rfind(quote)?;
    Some(s[open + quote.len_utf8()..inner_end].to_owned())
}

/// Whether `s` ends with `word` on an identifier boundary (the char before it, if any, is not an
/// identifier char) — i.e. `word` appears as a whole trailing token.
fn ends_with_word(s: &str, word: &str) -> bool {
    match s.strip_suffix(word) {
        Some(prefix) => prefix
            .chars()
            .next_back()
            .map_or(true, |c| !is_ident_char(c)),
        None => false,
    }
}

/// Whether `s` starts with `word` on an identifier boundary (the char after it, if any, is not an
/// identifier char) — i.e. `word` appears as a whole leading token.
fn starts_with_word(s: &str, word: &str) -> bool {
    match s.strip_prefix(word) {
        Some(suffix) => suffix.chars().next().map_or(true, |c| !is_ident_char(c)),
        None => false,
    }
}

/// The last whitespace-free identifier token in `s` (e.g. `UITable` from `local x = UITable`), or
/// `None` if the trailing token is not a valid identifier. Used to pull the assigned global out of
/// an `extends` line's left side and the receiver out of a `function R:method` header.
fn last_identifier(s: &str) -> Option<String> {
    let token = s.split_whitespace().next_back()?;
    if is_identifier(token) {
        Some(token.to_owned())
    } else {
        None
    }
}

/// Whether `s` is a non-empty Lua identifier (`[A-Za-z_][A-Za-z0-9_]*`).
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(is_ident_char)
}

/// Whether `c` may appear inside a Lua identifier.
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The workspace-wide index of Lua-declared widgets, aggregated per document.
///
/// Mirrors [`StyleIndex`](crate::style_index::StyleIndex): each Lua document contributes a
/// [`LuaWidgetDef`] list keyed by an opaque [`DocId`], and the server re-indexes one document at a
/// time ([`set_document`](Self::set_document)) or drops it ([`remove_document`](Self::remove_document))
/// as files change. Lookups fan out across every document, since the widget namespace is global.
///
/// Duplicate widget names across documents are all retained (the last-registered wins at runtime,
/// exactly as with styles); the merged accessors — [`parent_of`](Self::parent_of),
/// [`declares`](Self::declares) — combine every matching def.
#[derive(Debug, Default)]
pub struct LuaWidgetIndex {
    by_doc: HashMap<DocId, Vec<LuaWidgetDef>>,
}

impl LuaWidgetIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace **all** widget defs for one document (re-index on change).
    pub fn set_document(&mut self, doc: impl Into<DocId>, defs: Vec<LuaWidgetDef>) {
        self.by_doc.insert(doc.into(), defs);
    }

    /// Remove a document and its defs (e.g. on delete), returning them if present.
    pub fn remove_document(&mut self, doc: &DocId) -> Option<Vec<LuaWidgetDef>> {
        self.by_doc.remove(doc)
    }

    /// The defs a single document currently contributes, if it is indexed.
    #[must_use]
    pub fn document(&self, doc: &DocId) -> Option<&[LuaWidgetDef]> {
        self.by_doc.get(doc).map(Vec::as_slice)
    }

    /// Every widget def named `name` across **all** documents (duplicates are legal and all kept).
    #[must_use]
    pub fn lookup(&self, name: &str) -> Vec<&LuaWidgetDef> {
        self.iter().filter(|def| def.name == name).collect()
    }

    /// The Lua parent recorded for the widget `name`. `None` when `name` is unknown or was only seen
    /// through an `onStyleApply` with no `extends`.
    ///
    /// Duplicate declarations of the same widget in different documents are legal (e.g. a fork
    /// override), so — like [`style_index`](crate::style_index)'s `pick_base` — the winner is chosen
    /// **deterministically** by document id, not by unordered `HashMap` iteration, so the resolved
    /// ancestry never flickers between runs.
    #[must_use]
    pub fn parent_of(&self, name: &str) -> Option<&str> {
        let mut candidates: Vec<(&str, &str)> = self
            .by_doc
            .iter()
            .flat_map(|(doc, defs)| defs.iter().map(move |def| (doc.as_str(), def)))
            .filter(|(_, def)| def.name == name)
            .filter_map(|(doc, def)| def.lua_parent.as_deref().map(|p| (doc, p)))
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(b.0));
        candidates.first().map(|(_, parent)| *parent)
    }

    /// Whether the widget `name` declares the custom style property `prop` (in any matching def).
    /// This is the per-widget membership test the diagnostics pass consults; the base-chain walk is
    /// a later node's concern.
    #[must_use]
    pub fn declares(&self, name: &str, prop: &str) -> bool {
        self.iter()
            .filter(|def| def.name == name)
            .any(|def| def.custom_props.contains(prop))
    }

    /// Iterate every widget def across all documents.
    pub fn iter(&self) -> impl Iterator<Item = &LuaWidgetDef> {
        self.by_doc.values().flat_map(|defs| defs.iter())
    }

    /// The number of documents currently indexed.
    #[must_use]
    pub fn document_count(&self) -> usize {
        self.by_doc.len()
    }

    /// Whether the index holds no documents.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_doc.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connected_style_hook_declares_properties_on_the_connected_class() {
        // The engine's tooltip module attaches a *signal handler* to `UIWidget` rather than
        // overriding a method; the handler reads `styleNode.tooltip`, which is what makes `tooltip:`
        // a valid style property on every widget. Pass 2 (methods) never sees this shape.
        let src = "\
local function onWidgetStyleApply(widget, styleName, styleNode)
    if styleNode.tooltip then
        widget.tooltip = styleNode.tooltip
    end
    if styleNode.specialtooltip then
        widget.specialtooltip = styleNode.specialtooltip
    end
end

function g_tooltip.init()
    connect(UIWidget, {
        onStyleApply = onWidgetStyleApply,
        onHoverChange = onWidgetHoverChange
    })
end
";
        let defs = scan_widgets(src);
        let w = defs
            .iter()
            .find(|d| d.name == "UIWidget")
            .expect("the connected class is recorded");
        assert!(w.custom_props.contains("tooltip"));
        assert!(w.custom_props.contains("specialtooltip"));
    }

    #[test]
    fn a_style_hook_is_not_donated_to_a_preceding_connect() {
        // A `connect(A, {...})` with no style hook must NOT pick up the `onStyleApply` of a *later*
        // `connect(B, {...})`. Scanning blindly ahead would attribute B's properties to A.
        let src = "\
local function handler(widget, styleName, styleNode)
    if styleNode.tooltip then widget.tooltip = styleNode.tooltip end
end

function m.init()
    connect(UIButton, {
        onHoverChange = f,
        onDestroy = g
    })
    connect(UIWidget, {
        onStyleApply = handler
    })
end
";
        let defs = scan_widgets(src);
        let w = defs
            .iter()
            .find(|d| d.name == "UIWidget")
            .expect("the hook's own class");
        assert!(w.custom_props.contains("tooltip"));
        // UIButton declared no style hook, so it must declare no properties.
        assert!(
            defs.iter()
                .all(|d| d.name != "UIButton" || d.custom_props.is_empty()),
            "the later hook leaked onto UIButton: {defs:?}"
        );
    }

    #[test]
    fn a_single_line_connect_style_hook_still_resolves() {
        // The table-end guard must not cut the scan before the entry on the very same line.
        let src = "\
local function h(widget, styleName, styleNode)
    if styleNode.tooltip then widget.tooltip = styleNode.tooltip end
end
connect(UIWidget, { onStyleApply = h })
";
        let defs = scan_widgets(src);
        let w = defs
            .iter()
            .find(|d| d.name == "UIWidget")
            .expect("class recorded");
        assert!(w.custom_props.contains("tooltip"));
    }

    #[test]
    fn a_connect_without_a_style_hook_declares_nothing() {
        let src = "\
function m.init()
    connect(UIWidget, { onHoverChange = f, onDestroy = g })
end
";
        assert!(scan_widgets(src).is_empty());
    }

    fn props(def: &LuaWidgetDef) -> Vec<&str> {
        def.custom_props.iter().map(String::as_str).collect()
    }

    #[test]
    fn scans_extends_parent_and_on_style_apply_props() {
        // The canonical UITable shape: an `extends` parent plus a five-branch equality chain.
        let src = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'table-data' then
      foo()
    elseif name == 'column-style' then
      bar()
    elseif name == 'row-style' then
      baz()
    elseif name == 'header-column-style' then
      qux()
    elseif name == 'header-row-style' then
      quux()
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        let table = &defs[0];
        assert_eq!(table.name, "UITable");
        assert_eq!(table.lua_parent.as_deref(), Some("UIWidget"));
        assert_eq!(
            props(table),
            [
                "column-style",
                "header-column-style",
                "header-row-style",
                "row-style",
                "table-data",
            ]
        );
    }

    #[test]
    fn handles_single_and_double_quoted_literals() {
        let src = "\
Widget = extends(UIWidget, \"Widget\")

function Widget:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'single-quoted' then
    elseif name == \"double-quoted\" then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["double-quoted", "single-quoted"]);
    }

    #[test]
    fn reads_the_extends_parent_when_double_quoted_name() {
        // The registered name uses double quotes here (`"UIProgressBarSD"`); the parent is still
        // the first, bare-identifier argument.
        let src = "UIProgressBarSD = extends(UIWidget, \"UIProgressBarSD\")\n";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "UIProgressBarSD");
        assert_eq!(defs[0].lua_parent.as_deref(), Some("UIWidget"));
        assert!(defs[0].custom_props.is_empty());
    }

    #[test]
    fn a_widget_with_props_but_no_extends_has_no_parent() {
        // No `extends` line in the scanned text: props are captured, parent is None.
        let src = "\
function UIMinimap:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'autowalk' then
      self.autowalk = value
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "UIMinimap");
        assert_eq!(defs[0].lua_parent, None);
        assert_eq!(props(&defs[0]), ["autowalk"]);
    }

    #[test]
    fn a_file_with_no_on_style_apply_yields_no_props() {
        // An `extends` line but no `onStyleApply`: the widget is known (for the parent chain) but
        // contributes no custom properties.
        let src = "\
UIButton = extends(UIWidget, 'UIButton')

function UIButton:onClick()
  doSomething()
end
";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "UIButton");
        assert_eq!(defs[0].lua_parent.as_deref(), Some("UIWidget"));
        assert!(defs[0].custom_props.is_empty());
    }

    #[test]
    fn source_with_neither_construct_is_empty() {
        let src = "local x = 1\nprint('hello')\n";
        assert!(scan_widgets(src).is_empty());
    }

    #[test]
    fn multiple_widgets_in_one_file_are_separated() {
        // uitable.lua declares three widgets in one file; each keeps its own parent and props.
        let src = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'row-style' then
    end
  end
end

UITableRow = extends(UIWidget, 'UITableRow')

function UITableRow:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'even-background-color' then
    elseif name == 'odd-background-color' then
    end
  end
end

UITableHeaderColumn = extends(UIButton, 'UITableHeaderColumn')

function UITableHeaderColumn:onClick()
end
";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 3);

        let table = defs.iter().find(|d| d.name == "UITable").unwrap();
        assert_eq!(table.lua_parent.as_deref(), Some("UIWidget"));
        assert_eq!(props(table), ["row-style"]);

        let row = defs.iter().find(|d| d.name == "UITableRow").unwrap();
        assert_eq!(row.lua_parent.as_deref(), Some("UIWidget"));
        assert_eq!(
            props(row),
            ["even-background-color", "odd-background-color"]
        );

        let header = defs
            .iter()
            .find(|d| d.name == "UITableHeaderColumn")
            .unwrap();
        assert_eq!(header.lua_parent.as_deref(), Some("UIButton"));
        assert!(header.custom_props.is_empty());
    }

    #[test]
    fn props_from_one_body_do_not_bleed_into_the_next_widget() {
        // The body boundary must stop at the closing `end` so a later method's comparisons are not
        // attributed to the wrong widget.
        let src = "\
function UIScrollArea:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'vertical-scrollbar' then
    end
  end
end

function UIScrollArea:someOtherMethod(kind)
  if kind == 'not-a-style-prop' then
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(props(&defs[0]), ["vertical-scrollbar"]);
    }

    #[test]
    fn honors_a_non_default_loop_key_variable() {
        // The key variable is `k`, not `name`; only `k == '<lit>'` comparisons are props, and the
        // unrelated `other == 'ignored'` on a different variable is not collected.
        let src = "\
function W:onStyleApply(styleName, styleNode)
  for k, v in pairs(styleNode) do
    if k == 'real-prop' then
    elseif other == 'ignored' then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["real-prop"]);
    }

    #[test]
    fn captures_direct_dot_field_reads() {
        // The UIComboBox shape: `options`/`data` are read as direct fields (before and inside the
        // loop), the rest through `name == '...'`. All are custom props.
        let src = "\
function UIComboBox:onStyleApply(styleName, styleNode)
  if styleNode.options then
    for k, option in pairs(styleNode.options) do
    end
  end
  if styleNode.data then
  end
  for name, value in pairs(styleNode) do
    if name == 'mouse-scroll' then
    elseif name == 'menu-height' then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(
            props(&defs[0]),
            ["data", "menu-height", "mouse-scroll", "options"]
        );
    }

    #[test]
    fn captures_bracket_index_reads_including_hyphenated_keys() {
        // The UIMoveableTabBar shape: everything is read via `styleNode['...']`, and the keys are
        // hyphenated — which a bare field access cannot express.
        let src = "\
function UIMoveableTabBar:onStyleApply(styleName, styleNode)
  if styleNode['movable'] then
    self.tabsMoveable = styleNode['movable']
  end
  if styleNode['tab-spacing'] then
    self:setTabSpacing(styleNode['tab-spacing'])
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["movable", "tab-spacing"]);
    }

    #[test]
    fn honors_a_non_default_style_node_parameter_name() {
        // The style-node parameter is `sn`, not `styleNode`; direct reads must key off it.
        let src = "\
function W:onStyleApply(sname, sn)
  if sn.foo then
  end
  if sn['bar-baz'] then
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["bar-baz", "foo"]);
    }

    #[test]
    fn style_node_iteration_and_similar_names_are_not_field_reads() {
        // `pairs(styleNode)` is not a `.`/`[` read, and `myStyleNode` is a different variable — the
        // only real prop here is the `== '...'` one.
        let src = "\
function W:onStyleApply(styleName, styleNode)
  local myStyleNode = something()
  if myStyleNode.ignored then end
  for name, value in pairs(styleNode) do
    if name == 'real' then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["real"]);
    }

    #[test]
    fn honors_a_renamed_style_node_param_in_the_equality_chain() {
        // The style-node param is renamed (`sn`) AND the loop key is non-default (`k`): the loop-key
        // detection must key off the resolved node var (`pairs(sn)`), not the literal `styleNode`,
        // otherwise the `k == '...'` comparison is missed.
        let src = "\
function W:onStyleApply(sname, sn)
  for k, v in pairs(sn) do
    if k == 'real-prop' then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["real-prop"]);
    }

    #[test]
    fn accepts_the_reversed_comparison_order() {
        let src = "\
function W:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if 'flipped' == name then
    end
  end
end
";
        let defs = scan_widgets(src);
        assert_eq!(props(&defs[0]), ["flipped"]);
    }

    #[test]
    fn extends_with_non_identifier_first_arg_has_no_parent() {
        // A first argument that is not a bare identifier leaves the parent unresolved rather than
        // guessing.
        let src = "W = extends('notclass', 'W')\n";
        let defs = scan_widgets(src);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "W");
        assert_eq!(defs[0].lua_parent, None);
    }

    #[test]
    fn index_aggregates_across_documents() {
        let mut index = LuaWidgetIndex::new();
        index.set_document(
            "uitable.lua",
            scan_widgets("UITable = extends(UIWidget, 'UITable')\n"),
        );
        index.set_document(
            "uibutton.lua",
            scan_widgets("UIButton = extends(UIWidget, 'UIButton')\n"),
        );
        assert_eq!(index.document_count(), 2);

        assert_eq!(index.parent_of("UITable"), Some("UIWidget"));
        assert_eq!(index.parent_of("UIButton"), Some("UIWidget"));
        assert_eq!(index.parent_of("Missing"), None);

        assert_eq!(index.lookup("UITable").len(), 1);
        assert!(index.lookup("Missing").is_empty());
    }

    #[test]
    fn parent_of_is_deterministic_for_a_widget_declared_in_two_documents() {
        // The same widget with different `extends` parents in two files (e.g. a fork override) — the
        // winner is picked by document id, never by insertion or unordered-map order. Building the
        // index in BOTH insertion orders must resolve to the same parent: `a.lua` sorts before
        // `b.lua`, so its `UIWidget` wins regardless of which document was added first. A
        // last-writer or insertion-order implementation would disagree between the two indexes.
        let a = || scan_widgets("UITable = extends(UIWidget, 'UITable')\n");
        let b = || scan_widgets("UITable = extends(UIScrollArea, 'UITable')\n");

        let mut a_first = LuaWidgetIndex::new();
        a_first.set_document("a.lua", a());
        a_first.set_document("b.lua", b());

        let mut b_first = LuaWidgetIndex::new();
        b_first.set_document("b.lua", b());
        b_first.set_document("a.lua", a());

        assert_eq!(a_first.parent_of("UITable"), Some("UIWidget"));
        assert_eq!(b_first.parent_of("UITable"), Some("UIWidget"));
        // Both declarations are still retained by lookup.
        assert_eq!(a_first.lookup("UITable").len(), 2);
    }

    #[test]
    fn index_declares_checks_custom_props() {
        let src = "\
UITable = extends(UIWidget, 'UITable')

function UITable:onStyleApply(styleName, styleNode)
  for name, value in pairs(styleNode) do
    if name == 'column-style' then
    end
  end
end
";
        let mut index = LuaWidgetIndex::new();
        index.set_document("uitable.lua", scan_widgets(src));

        assert!(index.declares("UITable", "column-style"));
        assert!(!index.declares("UITable", "not-a-prop"));
        assert!(!index.declares("UIButton", "column-style"));
    }

    #[test]
    fn set_document_replaces_and_remove_drops() {
        let mut index = LuaWidgetIndex::new();
        let doc = DocId::new("w.lua");
        index.set_document(
            doc.clone(),
            scan_widgets("Old = extends(UIWidget, 'Old')\n"),
        );
        assert_eq!(index.parent_of("Old"), Some("UIWidget"));

        // Re-index the same doc: the old widget is gone.
        index.set_document(
            doc.clone(),
            scan_widgets("New = extends(UIWidget, 'New')\n"),
        );
        assert_eq!(index.parent_of("Old"), None);
        assert_eq!(index.parent_of("New"), Some("UIWidget"));
        assert_eq!(index.document_count(), 1);

        let removed = index.remove_document(&doc).expect("was present");
        assert_eq!(removed.len(), 1);
        assert!(index.is_empty());
        assert!(index.remove_document(&doc).is_none());
    }
}
