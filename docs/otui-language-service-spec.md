<!--
  Interface contract otui-lsp implements. Every rule is grounded in and cited to the OTClient
  engine's actual OTML/OTUI parsing behavior, kept here so the server's fidelity target travels
  with the code. Section numbers (§2.x, §4, §6, …) are referenced throughout otui-lsp.
-->

# OTUI Language Service — Interface Specification

## 1. Purpose & scope

This document specifies the **interfaces** for a language-intelligence service for OTUI/OTML — the
markup language exclusive to the OTClient game client. "Interfaces" means data contracts
and required behavior, not an implementation.

**Cross-cutting requirement: fidelity to the real OTClient engine.** Every capability this spec
defines — every diagnostic, every completion, every hover string, every go-to-definition target —
must match the engine's actual C++ parsing/resolution behavior exactly, including its specific
tolerances (e.g. silently ignoring an unrecognized property rather than erroring) and its specific
strictnesses (e.g. hard-erroring on tab-indented lines).

This spec covers four capability areas: **tokenizer/grammar** (§3), **diagnostics** (§4),
**symbols** — definitions, references, workspace symbols, which also covers hover (§5), and
**completion** (§6).

## 2. OTML/OTUI grammar reference (ground truth)

Sources: `src/framework/otml/otmlparser.cpp`, `otmlnode.{h,cpp}`, `otmldocument.{h,cpp}`,
`src/framework/ui/uiwidgetbasestyle.cpp`, `uiwidgetimage.cpp`, `uiwidgettext.cpp`,
`uimanager.cpp`, `uianchorlayout.cpp`, `uitranslator.cpp`, `src/framework/util/color.cpp`,
`src/framework/luaengine/luainterface.cpp`, `docs/otml-variables.md`, `docs/ai/otui.md`.

### 2.1 Lexical/document structure

- **Indentation**: exactly 2 spaces per depth level; tabs in leading whitespace are a hard error
  (`OTMLParser::getLineDepth`). Depth can only increase by exactly 1 level relative to the previous
  node; anything else is a hard error ("invalid indentation depth").
- **Node forms**, one per line:
  - `key: value` — a key/value pair (colon-split, with a carve-out so `http://`/`https://` values
    aren't mistaken for the key/value separator).
  - `key` alone (no colon) — a container node with indented children (a widget tag, a `$state`
    selector header, etc.).
  - `- item` — a list-item child (`[a, b, c]` inline arrays are far more common).
  - `~` as a value — explicit null; a null child hides/deletes an inherited child of the same tag
    when styles merge.
  - `[a, b, c]` — inline array literal, becomes multiple unnamed children.
  - `|`, `|-`, `|+` — YAML-style literal block scalars: `|` keeps exactly one trailing newline,
    `|-` strips all trailing blank lines, `|+` keeps them all. The mechanism for multi-line
    embedded Lua bodies (see 2.5).
  - Quoted strings (`"..."`/`'...'`) support escapes (`\\`, `\"`, `\t`, `\n`, `\'`).
- **Comments**: a line starting with `//` or `#` is skipped entirely. **Full-line only** — there is
  no trailing/inline comment support; a literal `#` inside a value is data (except the `&`-value
  carve-out, see 2.6).
- **"Unique" nodes**: a node that had an explicit `:` (or is a URL-form key) is "unique" — a later
  unique child with the same tag *replaces* (recursively merging children) an earlier same-tag
  child. This underlies both style inheritance and "later property wins" semantics within one file.
- **No file-include/import directive exists in OTML.** Cross-file composition happens only via the
  global style-inheritance namespace (2.2) and Lua-side module loading.

### 2.2 Style inheritance — `Name < Base`

`Name < Base` (optionally `#Name < Base` to "freeze"/lock the style after first definition) is not
OTML grammar per se — OTML parses it as a plain tag string; the `<` is interpreted in
`UIManager::importStyleFromOTML`.

- Every such declaration is registered into **one project-wide, global namespace**
  (`UIManager::m_styles`) — **not scoped to the declaring file**. Resolving `Name < Base` requires
  indexing every declaration across every loaded `.otui` file in the workspace.
- A base name starting with `"UI"` auto-resolves to a **built-in native widget class**
  (`UIWidget`, `UIButton`, `UICheckBox`, `UITextEdit`, `UIMiniWindow`, etc. — `UIManager::getStyle`),
  never to a file. Go-to-definition on such a base must present "built-in widget class," not
  "unresolved."
- An `.otui` file's one top-level node *without* a `<` in its tag is "the main widget to
  instantiate"; having more than one such node is a structural error
  (`UIManager::findMainWidgetNode`).

### 2.3 `id:` — the cross-reference backbone

Every widget with an `id:` becomes a literal field on its parent, in the C++ engine
(`UIWidget::setId`, which maintains a `m_childrenById` map) and in Lua. This is the basis for:

- Dotted Lua access: `controller.ui.someId.childId` — every identifier after `.ui.` is an `id:`
  value from the paired `.otui` file's tree (pairing convention: module directory name ==
  `.otui` base filename, e.g. `modules/game_inventory/inventory.otui` ↔
  `modules/game_inventory/inventory.lua`).
- `widget:getChildById('id')` / `widget:recursiveGetChildById('id')`.
- `anchors.<edge>: <id>.<edge>` targets (2.4).

### 2.4 Anchors

`anchors.<edge>: <targetId>.<targetEdge>` (`UIWidget::parseBaseStyle`), plus the shorthands
`anchors.fill: <target>` and `anchors.centerIn: <target>`. `<edge>` ∈ `top | bottom | left |
right | horizontalCenter | verticalCenter` (case-**and-whitespace**-insensitive,
`Fw::translateAnchorEdge` lowercases and strips spaces). `<targetId>` is either a magic keyword —
`parent`, `next`/`prev` (the adjacent siblings) — or a **direct sibling's** `id:` value: resolution
is `UIAnchor::getHookedWidget` → `parentWidget->getChildById(targetId)`, which searches only the
parent's **direct children** (not a recursive/ancestor lookup), so anchoring to an ancestor or a
non-sibling id silently fails to resolve at layout time. A missing target id is **not** a parse
error — it is a runtime no-op (only a bad *edge* or a malformed `id.edge` throws). Value `none`
removes an existing anchor for that edge.

### 2.5 Embedded Lua — `@tag:` event handlers

A child node whose tag starts with `@` (e.g. `@onClick:`) is compiled as Lua source
(`UIWidget::parseBaseStyle`) and bound as a field on the widget. The **value is raw Lua source**,
often spanning multiple lines via the `|`/`|-`/`|+` block-scalar forms. The engine **auto-wraps**
the value as `function(self) <body> end` **unless** the raw text already starts with the literal
keyword `function` (`LuaInterface::loadFunction`) — meaning `self` is an implicit, always-available
parameter inside every `@tag:` body. Known event names (worth first-class completion at the
`@`-key position): `onCreate, onSetup, onDestroy, onIdChange, onStyleApply, onWidthChange,
onHeightChange, onResize, onEnabled, onCheckChange, onPropertyChange, onGeometryChange,
onLayoutUpdate, onFocusChange, onChildFocusChange, onHoverChange, onTextHoverChange,
onVisibilityChange, onDragEnter, onDragLeave, onDragMove, onDrop, onKeyText, onKeyDown,
onKeyPress, onKeyUp, onMousePress, onMouseRelease, onMouseMove, onMouseWheel, onTextClick,
onClick, onDoubleClick, onTextChange, onFontChange, onTextAreaUpdate`.

### 2.6 `&tag:` — dual-purpose alias / Lua field

A node tag starting with `&` (e.g. `&primaryColor: #33AAFF`) is **simultaneously**:

1. An OTML **variable/alias**, referenced elsewhere via `$name` — resolution is **file-local only**
   (root-level `&` are "global" *within that document*, nested `&` are local to their subtree;
   there is no cross-file variable sharing — `OTMLParser::resolveVariablesRecursive`).
2. A **Lua-evaluated widget instance field** — the value is evaluated as a Lua expression
   (`UIWidget::parseBaseStyle` wraps as `__exp = (<value>)`), **except** a value starting with
   literal `#` is pushed as a plain string, not evaluated — the carve-out that lets `&color:
   #ff0000` hex literals survive.

Any hover/documentation surface for a `&tag:` key **must present both meanings** — this is a
documented, intentional ambiguity in the engine itself.

### 2.7 `!tag:` — live Lua expression

A tag prefixed `!` (e.g. `!text: tr('Some label')`) evaluates its value as a live Lua expression
at load time and substitutes the result (`UIWidget::applyStyle`). Fully generic — not limited to
the `tr()` translation idiom.

### 2.8 `$state` selector blocks

A child node whose tag starts with `$` is a conditional style block (`UIWidget::updateStyle`).
Syntax: a space-separated list of state names, each optionally `!`-negated, logically AND-combined
(`$hover !disabled:`, `$pressed:`, `$on:`). Valid names are a **closed set of exactly 14**
(`Fw::translateState`):

```
active, focus, hover, pressed, checked, disabled, on, first, middle, last, alternate, dragging, hidden, mobile
```

An unknown state name is **not an engine error** — it silently never matches. A genuine authoring
bug the engine will not flag; exactly the kind of thing a hint/completion should catch.

### 2.9 Color grammar

Fully implemented in `src/framework/util/color.cpp` (`Color::Color(string_view)` / `operator>>`):
`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa` hex forms; ~150 CSS named colors (binary-searched table)
plus several legacy engine-specific names (`alpha`, `black`, `white`, `darkRed`, `lightGray`,
etc.); functional `rgb(r,g,b)`, `rgba(r,g,b,a)`, `hsl(h,s%,l%)`, `hsla(...)`. There is no
`Color:name`-style call syntax — colors are always bare string values.

### 2.10 Property schema and its error-tolerance rule (important for §4)

Property parsing is a flat, linear dispatch (`UIWidget::parseBaseStyle`, plus
`parseImageStyle`/`parseTextStyle`/`parseCustomStyle`) over roughly 100 recognized keys spanning:
geometry (`x, y, pos, width, height, size, rect, min-*, max-*`), a CSS Flexbox subset (`display,
flex-direction, justify-content, align-items, gap, ...`), margin/padding (CSS 1/2/3/4-value
shorthand), borders (`border` shorthand `"<width> <style> <color>"` plus per-side variants),
image-* (`image-source, image-clip, image-border-*, image-color, ...`), text-* (`text,
text-align, font, ttf-font, ...`), anchors (2.4), and misc flags (`phantom, focusable, enabled,
visible, checked, on, draggable, opacity, rotation, clipping, ...`).

**The rule that must not be violated by any diagnostic (§4):** an unrecognized/misspelled property
name is **silently ignored by the engine — never an error, never even a warning at runtime.** Only
the validating properties actively validate their *value* and throw on malformed input: `border`,
`display`, `layout`, `anchors.*`, and every color-typed property (`color`, `background`,
`background-color`, `border-color*`, `icon-color`, `image-color`, `ttf-stroke-color`, ...) — the
engine's `Color(node->value())` throws on a value it cannot parse, just like the other validating
properties (mirrored by `diagnostics.rs`'s `check_property_value`, which validates the whole
color-property catalog, not just `border-color`). Every other property either applies cleanly or is silently dropped if
misspelled/unknown. Unknown property names are hints ("not a recognized property — ignored by the
engine"), never error-severity; malformed values for the validating properties are real errors;
malformed values for everything else are not validated at all.

## 3. Tokenizer / grammar interface

### 3.1 Host-agnostic token taxonomy

| Category | Covers |
|---|---|
| `comment` | full-line `//` or `#` comments (2.1) |
| `style.name` / `style.base` | the two sides of a `Name < Base` header (2.2) |
| `style.unique-marker` | the optional leading `#` in `#Name < Base` |
| `widget.tag` | a bare container/widget-tag line |
| `property.key` | a generic `key:` |
| `property.key.id` | specifically the `id:` key — a definition target |
| `property.key.anchor` | `anchors.<edge>:` keys |
| `selector.state.known` | a `$`-block state name inside the closed 14-name set (2.8) |
| `selector.state.unknown` | a `$`-block state name outside that set — render visibly distinct |
| `event.key` | an `@tag:` key |
| `alias-or-field.key` | an `&tag:` key |
| `expr.key` | a `!tag:` key |
| `color.literal` | hex/named/functional colors (2.9) |
| `string`, `number`, `null.literal` (`~`), `list.marker` (`-`) | standard literal/structural tokens |
| `embedded-lua.*` | inside an `@`/`&`/`!` value body — delegated to a real Lua tokenizer (3.3) |

A Monarch grammar, a TextMate grammar, or a tree-sitter grammar are different serializations of
this same list. (In `otui-lsp` the binding is a tree-sitter grammar with a C external scanner for
indentation; §3.1 categories map onto tree-sitter `highlights.scm` capture names, and the
embedded-Lua regions onto an `injections.scm` `lua` injection.)

### 3.3 Embedded Lua — realistic scope

Highlighting delegates the `@`/`&`/`!` value regions to a real Lua grammar (tree-sitter injection).
This gives correct **lexical** coloring (keywords, strings, comments, numbers) inside embedded
regions. It does **not** give nesting-aware indentation or semantic coloring of `self`/widget-field
references — those require a real Lua parse tree and are out of scope for the tokenizer (see §6's
note on deferred embedded-Lua intelligence).

## 4. Diagnostics interface

### 4.1 Contract

Per diagnostic:

```
severity: "error" | "warning" | "hint"
code: string            // stable id, e.g. "unknown-base", "tab-indentation", "unknown-property"
message: string
span: { startByte: usize, endByte: usize }   // byte offsets into the source
```

Byte offsets (not just line numbers) are required because several categories need sub-line
precision (e.g. flagging just the base-name token in a `Name < Base` header).

**UTF-8/UTF-16 integration note**: source text is naturally UTF-8 bytes, but the LSP's default
`PositionEncodingKind` is UTF-16-code-unit based. Any renderer must convert byte offset → UTF-16
offset before turning it into a line/column position, or diagnostics will be mis-placed by however
many multi-byte characters precede them on that line — the realistic trigger is a translated
`!text: tr('...')` value containing non-ASCII characters.

### 4.2 Required diagnostic categories, each traced to an engine behavior in §2

- **Parse-level (always `error`)**: tab indentation, odd (non-2-space-multiple) indentation,
  invalid depth jump, orphaned/undented node, malformed inline array or block-scalar (2.1).
- **Style-resolution level**:
  - Unknown base referenced in `Name < Base` (2.2) → **`warning`**, not error.
  - The file's root/main-widget instance itself resolving to an unknown style → **`error`**.
  - A `.otui` fragment with **no** root instance node (only `Name < Base` declarations) → **not a
    diagnostic at all** — a "style-only" file is valid.
- **Property level** (enforcing the 2.10 rule):
  - Unrecognized/misspelled property name → **`hint`** only, never `warning` or `error`.
  - Malformed value for one of the validating properties (`border`, `display`, `layout`,
    `anchors.*`, and every color-typed property) → **`error`**.
  - Malformed value for any other property → **no diagnostic**.
- **Selector level**: a `$state` name outside the closed 14-name set (2.8) → **`hint`**.

## 5. Symbols interface (definitions, references, workspace symbols, hover)

Hover is "what does the symbol under the cursor resolve to" — the same resolution go-to-definition
needs, rendered as descriptive text instead of a jump target.

### 5.1 Document symbols

Per-open-file `id:` tree (parent → child widget hierarchy keyed by `id:` value, per 2.3), shaped
for direct use as a `DocumentSymbol` hierarchy (`{name, kind, range, selectionRange, children}`).

### 5.2 Workspace symbols

The **global** `Name < Base` style-declaration namespace (2.2), indexed across every `.otui` file —
genuinely global per `UIManager::m_styles`, not per-file. Each entry: `{ name, base, file, span }`.
A base name starting with `UI` must be represented as "resolves to a built-in widget class, no
source location" — distinct from "unresolved" (a diagnostic, §4) and from "resolved to a file".

### 5.3 Go-to-definition targets

| Cursor is on... | Resolves against |
|---|---|
| A `Name < Base` header's base-name token | The workspace style index (5.2); `UI*` → built-in, no jump |
| An `anchors.<edge>` target id | The current file's `id:` tree; `parent`/`next`/`prev` are pseudo-targets |
| A Lua `self.ui.<id>...` dotted-chain segment, or a `getChildById('<id>')` string argument | The paired `.otui` file's `id:` tree (2.3) |

### 5.4 Find-references

For an `id:` declaration: combine (a) the declaring file's own anchor-target usages, and (b) a scan
of the paired `.lua` controller for the same Lua-reference shapes (dot chains and both
`getChildById` variants).

### 5.5 Hover content requirements

- Property key → schema description (2.10), value-kind, and whether it's one of the
  hard-error-validating properties or silently-ignored-if-misspelled.
- `id:` value → "this widget's id" plus a reference count (from 5.4).
- `&tag:` key → **both** meanings from 2.6, rendered together.
- `Name < Base` header's base name → resolved location (5.2) or "built-in widget class" for `UI*`.
- Anchor target identifier → the resolved direct-sibling's kind, or "not found".

## 6. Completion interface

A **context-dispatch table** — completions differ by where in the grammar the cursor sits:

| Cursor context | Completion source |
|---|---|
| Property-name position | The §2.10 property schema names |
| Property-value position (keyed by property) | Enum values for that property (e.g. `text-align` → `left\|center\|right`), the color-name list (2.9) for color-kind properties, or — for `anchors.*` — direct-sibling ids plus `parent\|next\|prev` |
| Inside a `$state` selector | The closed 14-name list (2.8) |
| `Name < Base` header's base slot | The workspace style index (5.2) names, plus the `UI*` built-in class list |
| `@tag:` **key** position | The known event-name list (2.5) |

**Explicitly out of scope**: completion *inside* an embedded Lua value body. This needs
virtual-document delegation to a real Lua language service (extract the span as a synthetic
document, run it through a Lua analyzer, map results back through the offset) — deferred.

## 7. Host-binding — standard LSP

The LSP method names this server implements: `textDocument/publishDiagnostics` (§4),
`textDocument/completion` (§6), `textDocument/hover` (§5.5), `textDocument/definition` (§5.3),
`textDocument/references` (§5.4), `textDocument/documentSymbol` (§5.1), `workspace/symbol` (§5.2).
LSP ranges are UTF-16-based by default, so the byte-offset→UTF-16 conversion from §4.1 applies.

## 9. Non-goals

- Deep embedded-Lua semantic intelligence (real completion/hover *inside* `@tag:`/`&tag:`/`!tag:`
  bodies) is out of scope and explicitly deferred (§6). Lexical embedded-Lua highlighting is in.

## 10. Reference fixtures

Real engine files useful for validating an implementation against real-world grammar usage:
`data/styles/10-buttons.otui`, `10-checkboxes.otui`, `10-windows.otui` (core widget style
declarations, `$state` selectors, inheritance chains), and the paired
`modules/game_inventory/inventory.otui` + `inventory.lua` (a real `.otui`/`.lua` module pair
exercising `id:` cross-references, `@onClick:` blocks including a multiline `|` Lua body, and
`$hover`/`$pressed`/`$checked`/`$disabled` state selectors).
