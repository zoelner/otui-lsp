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
    $._error_sentinel,
  ],

  // Newlines are handled by the external scanner (which runs first wherever a
  // `_newline` / `_indent` / `_dedent` is valid); listing `\n` here lets the
  // internal lexer absorb stray blank lines (e.g. a leading blank line) that no
  // structural token applies to, instead of erroring on them.
  extras: $ => [/[ \t\r\n]/],

  rules: {
    document: $ => repeat($._statement),

    _statement: $ => $._node,

    _block: $ => seq($._indent, repeat1($._statement), $._dedent),

    _node: $ => choice(
      $.comment,
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
    // `//` starts a comment anywhere; `#` only starts a comment when followed
    // by whitespace, which keeps `#Name < Base` freeze headers unambiguous.
    comment: $ => seq(
      token(choice(
        seq('//', /[^\n]*/),
        seq('#', /[ \t]/, /[^\n]*/),
      )),
      $._newline,
    ),

    // --- Name < Base style header (§2.2) ------------------------------------
    style_header: $ => seq(
      optional(field('freeze', alias('#', $.freeze_marker))),
      field('name', $.style_name),
      '<',
      field('base', $.style_base),
      choice($._newline, $._block),
    ),

    style_name: $ => token(IDENT),
    style_base: $ => token(IDENT),

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
    container: $ => seq(
      field('tag', $.tag),
      choice($._newline, $._block),
    ),

    tag: $ => token(IDENT),

    // --- list item ----------------------------------------------------------
    list_item: $ => seq(
      '-',
      field('value', optional($._value)),
      $._newline,
    ),

    // --- values -------------------------------------------------------------
    _value: $ => choice(
      $.null,
      $.inline_array,
      $.block_scalar,
      $._scalar_sequence,
    ),

    _scalar_sequence: $ => prec.left(repeat1($._scalar)),

    _scalar: $ => choice(
      $.color,
      $.number,
      $.boolean,
      $.variable,
      $.string,
      $.identifier,
    ),

    null: $ => '~',

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

    string: $ => token(choice(
      /"([^"\\\n]|\\.)*"/,
      /'([^'\\\n]|\\.)*'/,
    )),

    // A bare unquoted word inside a value: anything up to whitespace/comma or a
    // bracket (so inline-array delimiters are never swallowed).
    identifier: $ => token(prec(-1, /[^\s,\[\]{}|~][^\s,\[\]{}]*/)),
  },
});
