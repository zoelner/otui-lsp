; Syntax highlighting for OTUI/OTML.
; Maps the §3.1 host-agnostic token taxonomy onto standard tree-sitter capture
; names. See docs/otui-language-service-spec.md §3.1.

; --- comments (§2.1) --------------------------------------------------------
(comment) @comment

; --- Name < Base style headers (§2.2) ---------------------------------------
(style_header name: (style_name) @type)
(style_header "<" @operator)

; A base beginning with "UI" resolves to a built-in native widget class;
; anything else is a file-defined style. The two are mutually exclusive via
; predicates so the distinction holds regardless of capture/match order.
((style_base) @type.builtin
 (#match? @type.builtin "^UI"))
((style_base) @type
 (#not-match? @type "^UI"))

; --- widget / container tags (§2.1) -----------------------------------------
(container tag: (tag) @type)

; --- property keys (§2.10) --------------------------------------------------
(property key: (property_key) @property)

; id: is a definition target (§2.3), distinct from a generic property. The
; predicate makes the keyword classification order-independent (the `id_key`
; node is only ever the literal "id").
((id_key) @keyword
 (#eq? @keyword "id"))

; anchors.<edge>: (§2.4)
(anchor_property (anchor_keyword) @property)
(anchor_property edge: (anchor_edge) @property)
(anchor_property "." @punctuation.delimiter)
; The anchor target highlight is (re)stated after the generic `(identifier)`
; rule below, so ordering + priority let it win rather than being shadowed.

; --- @tag: / &tag: / !tag: (§2.5-2.7) ---------------------------------------
(event_property (event_name) @function)
(event_property "@" @punctuation.special)
(alias_property (alias_name) @property)
(alias_property "&" @punctuation.special)
(expr_property (expr_name) @property)
(expr_property "!" @punctuation.special)

; --- $state selectors (§2.8) ------------------------------------------------
(state_selector "$" @punctuation.special)
(state_negation) @operator

; The closed set of 14 engine-recognised state names is rendered as a constant;
; anything outside it is visibly distinct (an unknown state silently never
; matches at runtime — a bug a hint should catch).
((state_name) @constant.builtin
 (#any-of? @constant.builtin
  "active" "focus" "hover" "pressed" "checked" "disabled" "on"
  "first" "middle" "last" "alternate" "dragging" "hidden" "mobile"))
((state_name) @variable.parameter
 (#not-any-of? @variable.parameter
  "active" "focus" "hover" "pressed" "checked" "disabled" "on"
  "first" "middle" "last" "alternate" "dragging" "hidden" "mobile"))

; --- literals (§2.9, §2.1) --------------------------------------------------
(color) @constant
; A `&tag:` `#`-carve-out value (§2.6): a hex/color/string literal, not Lua.
(hash_literal) @constant
(number) @number
(boolean) @constant.builtin
(null) @constant.builtin
(string) @string
(variable) @variable
(identifier) @string
; An anchor target is a widget/edge reference, not a plain string. Stated after
; the generic `(identifier)` rule and given a higher priority so it wins over
; it regardless of match order.
((anchor_target target: (identifier) @variable)
 (#set! priority 105))
; An untyped scalar value spanning to end-of-line (§ faithful to parseNode).
(plain_value) @string

; --- structural punctuation -------------------------------------------------
(block_scalar_marker) @punctuation.special
(inline_array ["[" "]"] @punctuation.bracket)
(inline_array "," @punctuation.delimiter)
(list_item "-" @punctuation.delimiter)
":" @punctuation.delimiter
