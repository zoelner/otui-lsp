/**
 * Tree-sitter grammar for OTUI/OTML — the indentation-based UI markup language
 * used by the OTClient game client.
 *
 * The document is a tree of nodes, one node per line, nesting expressed with
 * exactly two spaces per depth level. Indentation is handled by an external C
 * scanner (src/scanner.c) which emits zero-width `_newline`, `_indent` and
 * `_dedent` tokens, plus a `block_scalar_content` token that swallows the raw
 * body of a `|` / `|-` / `|+` literal block.
 *
 * See docs/otui-language-service-spec.md §2 (grammar) and §3 (token taxonomy).
 */

// A key / tag fragment: letters, digits, underscore, dash (no dot — a dot in a
// key means a dotted `anchors.<edge>` form, handled separately).
const IDENT = /[A-Za-z_][A-Za-z0-9_\-]*/;
// A dotted identifier permitted in value position (e.g. `parent.top`, `$a.b`).
const DOTTED = /[A-Za-z_][A-Za-z0-9_.\-]*/;

module.exports = grammar({
  name: 'otui',

  externals: $ => [
    $._newline,
    $._indent,
    $._dedent,
    $.block_scalar_content,
    // A plain (untyped, unquoted) scalar value: the whole rest of the line
    // after the first `:`, trimmed. Emitted by the external scanner, which
    // reads the value and declines (so the internal lexer produces the typed
    // node instead) only when the value is EXACTLY one typed literal
    // (number / color / boolean / `~` / `$var` / quoted string) or begins a
    // `[` array / `|` block scalar. This is the lexical "anchor to newline"
    // that makes a typed literal win only when it is the whole value — token
    // precedence alone cannot express it (precedence outranks match length).
    $.plain_value,
    // A full-line comment (`//` / `#`), emitted by the external scanner ONLY at
    // a line start (faithful to otmlparser `parseLine`). Producing it externally
    // — rather than as an internal `token` in `extras` — is what makes a
    // trailing `//` / `#` after real tokens DATA, never a comment.
    $.comment,
    // A plain (untyped) container tag: the whole trimmed line when it has no
    // `:` separator and no `<` style-header marker (faithful to `parseNode`,
    // where a colon-less line is `tag = line`). Emitted by the scanner so it can
    // greedily reach end-of-line — `Foo # trailing` is the single tag
    // `Foo # trailing` — while the internal grammar still parses the structured
    // `Name < Base`, `$state:`, `@`/`&`/`!`/`anchors.`/`id:`/`key:` forms.
    $.tag,
    $._error_sentinel,
  ],

  // Newlines are handled by the external scanner (which runs first wherever a
  // `_newline` / `_indent` / `_dedent` is valid); listing `\n` here lets the
  // internal lexer absorb stray blank lines (e.g. a leading blank line) that no
  // structural token applies to, instead of erroring on them.
  //
  // Full-line `//` / `#` comments are `extras`: they may appear between
  // statements at any indentation and are indentation-neutral (they never open
  // or close a block). `$.comment` is an EXTERNAL token (see `externals`): the
  // scanner emits it ONLY at a line start, so a trailing `//` / `#` after real
  // tokens is never mistaken for a comment. The scanner computes the block
  // structure from the *next real line* by peeking ahead, keeping the comment's
  // own column from opening or closing a block.
  extras: $ => [/[ \t\r\n]/, $.comment],

  rules: {
    document: $ => repeat($._statement),

    _statement: $ => $._node,

    _block: $ => seq($._indent, repeat1($._statement), $._dedent),

    _node: $ => choice(
      $.style_header,
      $.state_selector,
      $.event_property,
      $.alias_property,
      $.expr_property,
      $.anchor_property,
      $.id_property,
      $.list_item,
      $.property,
      $.container,
    ),

    // --- comments (full line only) ------------------------------------------
    // `$.comment` is an EXTERNAL token (declared in `externals`, produced by
    // src/scanner.c). Faithful to the OTClient OTML parser (`parseLine`): after
    // trimming leading whitespace, a line that starts with `//` or `#` is a
    // comment — UNCONDITIONALLY (`#Panel < UIWidget` is a comment). Because the
    // scanner emits it ONLY at a line start, a mid-line / trailing `//` or `#`
    // after real tokens is NEVER a comment; it is data (consumed by the greedy
    // `tag` / `style_base` / `plain_value` / `lua_value` tokens).

    // --- Name < Base style header (§2.2) ------------------------------------
    // Just `Name < Base` — inheritance only (`importStyleFromOTML` splits on
    // `<`). There is no freeze marker: a leading `#` makes the line a comment.
    style_header: $ => seq(
      field('name', $.style_name),
      '<',
      field('base', $.style_base),
      choice($._newline, $._block),
    ),

    style_name: $ => token(IDENT),
    // The base is the rest of the line after `<`, trimmed. Faithful to
    // `parseNode` (a colon-less line is a whole-line tag), a trailing `//` / `#`
    // is part of the base — data, not a comment — so `Name < UIWidget # x`
    // yields no comment node. For the common `Name < Base` case the base is just
    // `Base`.
    style_base: $ => token(/\S([^\n]*\S)?/),

    // --- $state selector block (§2.8) ---------------------------------------
    state_selector: $ => seq(
      '$',
      repeat1($.state),
      ':',
      choice($._newline, $._block),
    ),

    state: $ => seq(
      optional(field('negated', alias('!', $.state_negation))),
      field('name', alias(token.immediate(IDENT), $.state_name)),
    ),

    // --- @tag: / &tag: / !tag: Lua-bearing properties (§2.5-2.7) ------------
    // The post-colon value of these is raw Lua source. An inline (single-line)
    // value is captured verbatim as ONE `lua_value` node — it may contain
    // commas, parens, quotes, dots, `#` (Lua length operator), etc. — rather
    // than being split into scalar atoms. Multi-line bodies keep using the
    // `|` / `|-` / `|+` block-scalar form. Both are the injection targets for
    // the embedded-Lua grammar (see queries/injections.scm).
    event_property: $ => seq(
      field('key', seq('@', alias(token.immediate(IDENT), $.event_name))),
      ':',
      field('value', optional(choice($.block_scalar, $.lua_value))),
      choice($._newline, $._block),
    ),

    // `&tag:` values are Lua too, EXCEPT the §2.6 carve-out: a value starting
    // with a literal `#` is pushed as a plain string (a color/hex literal),
    // never evaluated — so it becomes a `hash_literal` node and is NOT
    // lua-injected. Everything else is inline Lua.
    alias_property: $ => seq(
      field('key', seq('&', alias(token.immediate(IDENT), $.alias_name))),
      ':',
      field('value', optional(choice(
        $.block_scalar,
        $.hash_literal,
        $.lua_value,
      ))),
      choice($._newline, $._block),
    ),

    expr_property: $ => seq(
      field('key', seq('!', alias(token.immediate(IDENT), $.expr_name))),
      ':',
      field('value', optional(choice($.block_scalar, $.lua_value))),
      choice($._newline, $._block),
    ),

    // --- anchors.<edge>: <target> (§2.4) ------------------------------------
    // The object is specifically the literal keyword `anchors`; a generic
    // dotted key (`foo.left:`) is NOT an anchor.
    anchor_property: $ => seq(
      field('object', alias('anchors', $.anchor_keyword)),
      token.immediate('.'),
      field('edge', alias(token.immediate(IDENT), $.anchor_edge)),
      ':',
      field('value', optional($.anchor_target)),
      $._newline,
    ),

    anchor_target: $ => seq(
      field('target', alias(token(DOTTED), $.identifier)),
    ),

    // --- id: (§2.3) ---------------------------------------------------------
    id_property: $ => seq(
      field('key', alias('id', $.id_key)),
      ':',
      field('value', optional($._value)),
      $._newline,
    ),

    // --- generic key: value -------------------------------------------------
    property: $ => seq(
      field('key', $.property_key),
      ':',
      field('value', optional($._value)),
      choice($._newline, $._block),
    ),

    property_key: $ => token(IDENT),

    // --- bare container tag -------------------------------------------------
    // `$.tag` is an EXTERNAL token (declared in `externals`): the scanner emits
    // the whole trimmed line as the tag when the line has no `:` separator and
    // no `<` style-header marker, so `Foo # trailing` is a single tag.
    container: $ => seq(
      field('tag', $.tag),
      choice($._newline, $._block),
    ),

    // --- list item ----------------------------------------------------------
    list_item: $ => seq(
      '-',
      field('value', optional($._value)),
      $._newline,
    ),

    // --- values -------------------------------------------------------------
    // Faithful to `parseNode`: the value is the ENTIRE remainder of the line
    // after the first `:`, trimmed (`line.substr(dotsPos + 1)`). So an
    // unquoted, untyped scalar is one `plain_value` node spanning to
    // end-of-line — `text: Hello World` is the single value `Hello World`, and
    // `width: 10 // x` is the single value `10 // x` (the `//` is data).
    //
    // A typed literal wins only when it is the WHOLE value: color, number,
    // boolean, `~` null, `$var`, quoted string, `[..]` inline array, or a
    // `|`/`|-`/`|+` block scalar. Otherwise the value is the external
    // `plain_value` (the greedy rest-of-line, decided by the scanner).
    _value: $ => choice(
      $.null,
      $.inline_array,
      $.block_scalar,
      $.color,
      $.number,
      $.boolean,
      $.variable,
      $.string,
      $.plain_value,
    ),

    null: $ => token(prec(2, '~')),

    inline_array: $ => seq(
      '[',
      optional(seq(
        $._array_item,
        repeat(seq(',', $._array_item)),
        optional(','),
      )),
      ']',
    ),

    _array_item: $ => choice(
      $.color,
      $.number,
      $.boolean,
      $.variable,
      $.string,
      $.identifier,
    ),

    block_scalar: $ => seq(
      field('marker', $.block_scalar_marker),
      optional($.block_scalar_content),
    ),

    block_scalar_marker: $ => token(choice('|', '|-', '|+')),

    // A whole single-line inline Lua value (`@`/`!`/`&` bodies): everything from
    // the first non-space after `:` up to end-of-line, as one raw token. Lowest
    // precedence so a `|` block marker, a `#` carve-out literal, or the null `~`
    // still win where they apply.
    lua_value: $ => token(prec(-1, /[^ \t\r\n][^\r\n]*/)),

    // A `&tag:` value beginning with `#` (§2.6 carve-out): a hex/color/string
    // literal pushed verbatim, never Lua-evaluated. Higher precedence than
    // `lua_value` so it claims any `#`-leading alias value.
    hash_literal: $ => token(prec(1, /#[^\r\n]*/)),

    // color literals (§2.9): hex + functional forms
    color: $ => token(prec(3, choice(
      /#[0-9a-fA-F]{8}/,
      /#[0-9a-fA-F]{6}/,
      /#[0-9a-fA-F]{4}/,
      /#[0-9a-fA-F]{3}/,
      /(rgba?|hsla?)\([^)\n]*\)/,
    ))),

    number: $ => token(prec(2, /-?\d+(\.\d+)?%?/)),

    boolean: $ => token(prec(2, choice('true', 'false'))),

    // $name variable reference (resolved from a matching &tag:)
    variable: $ => token(prec(2, /\$[A-Za-z_][A-Za-z0-9_.\-]*/)),

    string: $ => token(prec(2, choice(
      /"([^"\\\n]|\\.)*"/,
      /'([^'\\\n]|\\.)*'/,
    ))),

    // A bare unquoted word inside a value: anything up to whitespace/comma or a
    // bracket (so inline-array delimiters are never swallowed).
    identifier: $ => token(prec(-1, /[^\s,\[\]{}|~][^\s,\[\]{}]*/)),
  },
});
