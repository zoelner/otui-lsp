; Embedded-Lua injection for OTUI/OTML (§2.5-2.7, §3.3).
;
; The value bodies of `@tag:` (event handler), `!tag:` (live expression) and
; `&tag:` (alias / Lua field) tags are raw Lua source, as are the `|` / `|-` /
; `|+` block scalars used to carry multi-line Lua. Delegate their lexical
; highlighting to a real Lua grammar.

; --- @tag: event handlers ---------------------------------------------------
(event_property
  value: (block_scalar (block_scalar_content) @injection.content)
  (#set! injection.language "lua"))

(event_property
  value: (identifier) @injection.content
  (#set! injection.language "lua"))

(event_property
  value: (string) @injection.content
  (#set! injection.language "lua"))

; --- !tag: live expressions -------------------------------------------------
(expr_property
  value: (block_scalar (block_scalar_content) @injection.content)
  (#set! injection.language "lua"))

(expr_property
  value: (identifier) @injection.content
  (#set! injection.language "lua"))

(expr_property
  value: (string) @injection.content
  (#set! injection.language "lua"))

; --- &tag: alias / Lua field ------------------------------------------------
; A `&` value beginning with `#` is pushed as a plain string, not evaluated
; (§2.6) — those parse as a (color) or plain identifier and are left alone here;
; block-scalar and expression bodies are injected.
(alias_property
  value: (block_scalar (block_scalar_content) @injection.content)
  (#set! injection.language "lua"))
