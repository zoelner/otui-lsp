// External scanner for OTUI/OTML indentation.
//
// Emits the zero-width structural tokens the grammar declares in `externals`:
//   NEWLINE  - end of a logical line at the same depth
//   INDENT   - a new, deeper block begins
//   DEDENT   - a block ends (one per level closed)
// plus BLOCK_SCALAR_CONTENT, which greedily consumes the raw indented body of a
// `|` / `|-` / `|+` literal block (used for embedded Lua) as a single token.
//
// The technique is the standard indent-stack used by the Python/YAML grammars:
// keep a stack of indentation widths, compare the next line's indent against the
// top of the stack, and serialize/deserialize the stack across incremental
// re-parses. Multiple dedents at one boundary are queued and emitted one call at
// a time. The scanner never hard-errors on malformed input (tabs, odd depth);
// fidelity checks live in otui-core, not here.

#include <tree_sitter/parser.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

enum TokenType {
  NEWLINE,
  INDENT,
  DEDENT,
  BLOCK_SCALAR_CONTENT,
  PLAIN_VALUE,
  COMMENT,
  TAG,
  ERROR_SENTINEL,
};

typedef struct {
  uint32_t len;
  uint32_t cap;
  uint16_t *data;
  uint16_t queued_dedents;
} Scanner;

// Grow the indent stack by one, guarding against integer overflow and a failed
// reallocation. Returns false (leaving the prior buffer intact) if the stack
// cannot grow; callers fail safe rather than dereference a NULL pointer.
static inline bool push(Scanner *s, uint16_t v) {
  if (s->len == s->cap) {
    uint32_t new_cap = s->cap ? s->cap * 2 : 8;
    // Overflow guards: the doubling must not wrap uint32_t, and the byte size
    // must fit size_t.
    if (new_cap < s->cap || new_cap > (uint32_t)(SIZE_MAX / sizeof(uint16_t))) {
      return false;
    }
    uint16_t *new_data =
        (uint16_t *)realloc(s->data, (size_t)new_cap * sizeof(uint16_t));
    if (new_data == NULL) {
      return false; // keep s->data usable; caller bails
    }
    s->data = new_data;
    s->cap = new_cap;
  }
  s->data[s->len++] = v;
  return true;
}

static inline uint16_t top(Scanner *s) {
  return s->len ? s->data[s->len - 1] : 0;
}

static inline void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static inline void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

// Consume the raw body of a block scalar: every following line indented deeper
// than `ref` (the key line's indent), plus interspersed blank lines. Stops
// before the first line at indent <= ref. Leaves the terminating newline of the
// last content line unconsumed so the grammar can still see a NEWLINE.
static bool scan_block_scalar(TSLexer *lexer, uint16_t ref) {
  // Skip trailing spaces on the marker line.
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\r') {
    skip(lexer);
  }
  if (lexer->lookahead != '\n') {
    return false; // nothing on following lines
  }

  bool has_content = false;
  for (;;) {
    if (lexer->lookahead != '\n') {
      break;
    }
    advance(lexer); // consume the newline ending the previous line

    // Measure indentation of the new line.
    uint32_t indent = 0;
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
      indent++;
      advance(lexer);
    }

    if (lexer->eof(lexer)) {
      break;
    }
    if (lexer->lookahead == '\n') {
      // Blank line: part of the block, keep going.
      continue;
    }
    if (indent <= ref) {
      // Dedented line: block ends here (this newline is not ours).
      break;
    }

    // A genuine content line: consume it up to (not including) its newline.
    has_content = true;
    while (lexer->lookahead != '\n' && !lexer->eof(lexer)) {
      advance(lexer);
    }
    lexer->mark_end(lexer); // token ends after this line's last char
  }

  if (has_content) {
    lexer->result_symbol = BLOCK_SCALAR_CONTENT;
    return true;
  }
  return false;
}

// Peek ahead across the rest of the current comment line plus any following
// blank or comment lines to find the indentation (leading-space count) of the
// next *real* content line. Used to keep comments indentation-neutral: a
// comment's own column never opens or closes a block; the block a comment sits
// in is decided by the next non-comment, non-blank line instead.
//
// Called with the lexer already positioned one char *into* the comment marker
// (just past the first `/` or `#`). Only ever `advance`s (peeks) and never calls
// `mark_end`, so the caller's earlier `mark_end` at the comment's start is what
// bounds the emitted structural token. Returns 0 at EOF.
static uint32_t peek_next_real_indent(TSLexer *lexer) {
  for (;;) {
    // Consume to the end of the current line.
    while (lexer->lookahead != '\n' && !lexer->eof(lexer)) {
      advance(lexer);
    }
    if (lexer->eof(lexer)) {
      return 0;
    }
    advance(lexer); // consume the newline

    // Measure this line's indentation.
    uint32_t indent = 0;
    while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
      indent++;
      advance(lexer);
    }

    if (lexer->eof(lexer)) {
      return 0;
    }
    if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
      continue; // blank line: skip
    }
    if (lexer->lookahead == '/') {
      advance(lexer);
      if (lexer->lookahead == '/') {
        continue; // another full-line `//` comment: skip
      }
      return indent; // lone `/`: real content
    }
    if (lexer->lookahead == '#') {
      continue; // another full-line `#` comment: skip (unconditional)
    }
    return indent; // real content
  }
}

// --- plain-value scanning ---------------------------------------------------
//
// A regular property/id/list value is the whole rest of the line after the
// first `:`, trimmed (faithful to otmlparser `parseNode`: `line.substr(dotsPos
// + 1)`). It is emitted as a single PLAIN_VALUE token — UNLESS the trimmed
// value is exactly one typed literal, in which case the scanner declines and
// the internal lexer produces the typed node (color/number/boolean/`~`/`$var`/
// string) or the grammar parses the `[` array / `|` block scalar. This lexical
// decision is what makes a typed literal win only when it is the WHOLE value.

static bool is_hex(char c) {
  return (c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') ||
         (c >= 'A' && c <= 'F');
}
static bool is_digit(char c) { return c >= '0' && c <= '9'; }

// number: -?\d+(\.\d+)?%?
static bool is_number(const char *b, uint32_t n) {
  uint32_t i = 0;
  if (i < n && b[i] == '-') i++;
  uint32_t d = 0;
  while (i < n && is_digit(b[i])) { i++; d++; }
  if (d == 0) return false;
  if (i < n && b[i] == '.') {
    i++;
    uint32_t f = 0;
    while (i < n && is_digit(b[i])) { i++; f++; }
    if (f == 0) return false;
  }
  if (i < n && b[i] == '%') i++;
  return i == n;
}

// hex color: #[0-9a-fA-F]{3,4,6,8}
static bool is_hex_color(const char *b, uint32_t n) {
  if (n < 4 || b[0] != '#') return false;
  uint32_t h = n - 1;
  if (h != 3 && h != 4 && h != 6 && h != 8) return false;
  for (uint32_t i = 1; i < n; i++) {
    if (!is_hex(b[i])) return false;
  }
  return true;
}

static bool starts_with(const char *b, uint32_t n, const char *p) {
  uint32_t i = 0;
  for (; p[i]; i++) {
    if (i >= n || b[i] != p[i]) return false;
  }
  return true;
}

// functional color: (rgb|rgba|hsl|hsla)\([^)]*\) spanning the whole value.
static bool is_func_color(const char *b, uint32_t n) {
  uint32_t open;
  if (starts_with(b, n, "rgba(") || starts_with(b, n, "hsla(")) {
    open = 5;
  } else if (starts_with(b, n, "rgb(") || starts_with(b, n, "hsl(")) {
    open = 4;
  } else {
    return false;
  }
  if (b[n - 1] != ')') return false;
  // No `)` before the final one (grammar uses [^)]*).
  for (uint32_t i = open; i < n - 1; i++) {
    if (b[i] == ')') return false;
  }
  return true;
}

// $name variable: \$[A-Za-z_][A-Za-z0-9_.\-]*
static bool is_variable(const char *b, uint32_t n) {
  if (n < 2 || b[0] != '$') return false;
  char c = b[1];
  if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_'))
    return false;
  for (uint32_t i = 2; i < n; i++) {
    c = b[i];
    if (!((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
          (c >= '0' && c <= '9') || c == '_' || c == '.' || c == '-'))
      return false;
  }
  return true;
}

// quoted string spanning the whole value: "..." or '...' with \-escapes and no
// unescaped closing quote before the end (grammar: ([^q\\\n]|\\.)*).
static bool is_string(const char *b, uint32_t n) {
  if (n < 2) return false;
  char q = b[0];
  if (q != '"' && q != '\'') return false;
  uint32_t i = 1;
  while (i < n) {
    if (b[i] == '\\') {
      i += 2;
      continue;
    }
    if (b[i] == q) return i == n - 1;
    i++;
  }
  return false;
}

static bool is_typed_literal(const char *b, uint32_t n) {
  if (n == 1 && b[0] == '~') return true;                 // null
  if (n == 4 && starts_with(b, n, "true")) return true;   // boolean
  if (n == 5 && starts_with(b, n, "false")) return true;  // boolean
  return is_number(b, n) || is_hex_color(b, n) || is_func_color(b, n) ||
         is_variable(b, n) || is_string(b, n);
}

// Outcome of a plain-value scan.
enum PlainResult {
  PLAIN_EMITTED,  // PLAIN_VALUE produced; the scanner should return true
  PLAIN_INTERNAL, // typed literal / `[` / `|`: let the internal lexer handle it
                  //   (the scanner must return false so the lexer resets)
  PLAIN_EMPTY,    // no value: fall through to the newline/indent scan
};

// Read the trimmed rest-of-line value (faithful to `parseNode`).
static enum PlainResult scan_plain_value(TSLexer *lexer) {
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
    skip(lexer);
  }
  if (lexer->lookahead == '\n' || lexer->lookahead == '\r' ||
      lexer->eof(lexer)) {
    return PLAIN_EMPTY;
  }
  if (lexer->lookahead == '[' || lexer->lookahead == '|') {
    return PLAIN_INTERNAL; // inline array / block scalar
  }

  char buf[512];
  uint32_t n = 0;         // bytes buffered
  uint32_t content = 0;   // buffer length up to the last non-space char
  bool overflow = false;
  for (;;) {
    int32_t c = lexer->lookahead;
    if (c == '\n' || c == '\r' || lexer->eof(lexer)) {
      break;
    }
    if (n < sizeof(buf)) {
      buf[n] = (char)c;
    } else {
      overflow = true;
    }
    n++;
    advance(lexer);
    if (c != ' ' && c != '\t') {
      content = n;             // last non-space extends the trimmed content
      lexer->mark_end(lexer);  // token ends after the last non-space char
    }
  }

  if (!overflow && content <= sizeof(buf) && is_typed_literal(buf, content)) {
    return PLAIN_INTERNAL; // the whole value is a single typed literal
  }
  lexer->result_symbol = PLAIN_VALUE;
  return PLAIN_EMITTED;
}

// --- line-start classification ----------------------------------------------
//
// Called only at a statement start (when TAG is a valid symbol). Faithful to
// the OTML parser's `parseLine` / `parseNode`, which split a line by looking for
// the FIRST `:` and treat a colon-less, non-list line as a whole-line tag:
//
//   * a line whose first non-space char is `#`, or begins with `//`, is a
//     comment (line-start ONLY, unconditional) — emitted as COMMENT here so a
//     trailing `#`/`//` elsewhere can never be mistaken for one;
//   * a line whose first non-space char is `-` is a list item — declined so the
//     internal grammar parses it;
//   * a line containing `:` (a key/value separator) or `<` (the tooling-modelled
//     `Name < Base` style header) is a structured form — declined;
//   * otherwise the WHOLE trimmed line is a container tag, consumed to
//     end-of-line so `Foo # trailing` is the single tag `Foo # trailing`.
//
// Never crosses a newline: it classifies only the current line. Leading
// same-line spaces/tabs are skipped. Returns PLAIN_EMPTY (fall through to the
// indentation scan) for a blank position.
static enum PlainResult scan_line_start(TSLexer *lexer,
                                        const bool *valid_symbols) {
  // Skip leading blanks. This runs only at a statement start (TAG valid), where
  // the preceding structural token has already emitted any INDENT/DEDENT and
  // positioned us at the content; leading newlines only occur at the document
  // start (base indent), so crossing them here is safe.
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\n' || lexer->lookahead == '\r') {
    skip(lexer);
  }

  int32_t c = lexer->lookahead;
  if (lexer->eof(lexer)) {
    return PLAIN_EMPTY;
  }

  // `#...` full-line comment.
  if (c == '#') {
    if (!valid_symbols[COMMENT]) {
      return PLAIN_EMPTY;
    }
    while (lexer->lookahead != '\n' && !lexer->eof(lexer)) {
      advance(lexer);
    }
    lexer->mark_end(lexer);
    lexer->result_symbol = COMMENT;
    return PLAIN_EMITTED;
  }

  bool tag_started = false; // a lone leading '/' already consumed as tag content
  if (c == '/') {
    if (valid_symbols[COMMENT]) {
      advance(lexer);
      if (lexer->lookahead == '/') {
        // `//...` full-line comment.
        while (lexer->lookahead != '\n' && !lexer->eof(lexer)) {
          advance(lexer);
        }
        lexer->mark_end(lexer);
        lexer->result_symbol = COMMENT;
        return PLAIN_EMITTED;
      }
      // A lone '/': the first char of a plain tag. It is already consumed;
      // continue scanning the rest as a tag below.
      if (!valid_symbols[TAG]) {
        return PLAIN_INTERNAL; // let the internal lexer take over from the start
      }
      tag_started = true;
    } else if (!valid_symbols[TAG]) {
      return PLAIN_EMPTY;
    }
    // COMMENT invalid but TAG valid: leave '/' unconsumed; the loop reads it.
  } else if (c == '-') {
    return PLAIN_INTERNAL; // list item — the internal grammar parses it
  }

  if (!valid_symbols[TAG]) {
    return PLAIN_EMPTY;
  }

  // Plain container tag: consume to end-of-line, right-trimmed. Decline if a
  // `:` or `<` appears (a structured key/value or style header).
  bool content = tag_started;
  if (tag_started) {
    lexer->mark_end(lexer);
  }
  for (;;) {
    int32_t ch = lexer->lookahead;
    if (ch == '\n' || ch == '\r' || lexer->eof(lexer)) {
      break;
    }
    if (ch == ':' || ch == '<') {
      return PLAIN_INTERNAL; // structured form: internal grammar parses it
    }
    advance(lexer);
    if (ch != ' ' && ch != '\t') {
      content = true;
      lexer->mark_end(lexer); // token ends after the last non-space char
    }
  }
  if (!content) {
    return PLAIN_EMPTY;
  }
  lexer->result_symbol = TAG;
  return PLAIN_EMITTED;
}

bool tree_sitter_otui_external_scanner_scan(void *payload, TSLexer *lexer,
                                            const bool *valid_symbols) {
  Scanner *s = (Scanner *)payload;

  // In error recovery let the internal lexer take over.
  if (valid_symbols[ERROR_SENTINEL]) {
    return false;
  }

  // Emit any dedents queued from a previous boundary (zero-width, no advance).
  if (s->queued_dedents > 0) {
    if (valid_symbols[DEDENT]) {
      s->queued_dedents--;
      if (s->len > 1) {
        s->len--;
      }
      lexer->result_symbol = DEDENT;
      return true;
    }
    s->queued_dedents = 0;
  }

  if (valid_symbols[BLOCK_SCALAR_CONTENT]) {
    if (scan_block_scalar(lexer, top(s))) {
      return true;
    }
    // fall through to normal indentation handling
  }

  // A plain value is valid only in value position; try it before the
  // indentation scan.
  if (valid_symbols[PLAIN_VALUE]) {
    enum PlainResult r = scan_plain_value(lexer);
    if (r == PLAIN_EMITTED) {
      return true;
    }
    if (r == PLAIN_INTERNAL) {
      // A `[` array, `|` block scalar, or a lone typed literal: return false so
      // tree-sitter resets the lexer and the internal lexer produces the node.
      return false;
    }
    // PLAIN_EMPTY: fall through to the newline/indent scan below.
  }

  // At a statement start, classify the line: emit a line-start COMMENT, emit a
  // whole-line container TAG, or decline (let the internal grammar parse a
  // structured form). Gated on TAG so it never runs in a value position — a
  // `#`/`//` mid-value is data, handled by plain_value / lua_value / hash_literal.
  if (valid_symbols[TAG]) {
    enum PlainResult r = scan_line_start(lexer, valid_symbols);
    if (r == PLAIN_EMITTED) {
      return true;
    }
    if (r == PLAIN_INTERNAL) {
      return false; // structured form / list item: reset for the internal lexer
    }
    // PLAIN_EMPTY: fall through to the newline/indent scan below.
  }

  bool found_line_end = false;
  uint32_t indent = 0;

  for (;;) {
    if (lexer->lookahead == '\n') {
      found_line_end = true;
      indent = 0;
      skip(lexer);
    } else if (lexer->lookahead == ' ') {
      indent++;
      skip(lexer);
    } else if (lexer->lookahead == '\r') {
      skip(lexer);
    } else if (lexer->lookahead == '\t') {
      indent++; // tolerate; otui-core flags tab indentation
      skip(lexer);
    } else if (lexer->eof(lexer)) {
      found_line_end = true;
      indent = 0;
      lexer->mark_end(lexer);
      break;
    } else if (lexer->lookahead == '/' && found_line_end) {
      // A `//` comment line reached while scanning across line ends. Mark the
      // structural token's end at the `/` (zero-width) and let the next real
      // line decide this line's block structure, keeping comments
      // indentation-neutral. The comment's own bytes are emitted as a COMMENT
      // token on the following scan (scan_line_start). `found_line_end` gates
      // this so a mid-line `/` in a value position stays data.
      lexer->mark_end(lexer);
      advance(lexer);
      if (lexer->lookahead == '/') {
        indent = peek_next_real_indent(lexer);
        break; // emit the structural token here; COMMENT follows next scan
      }
      break; // a lone '/', let the internal lexer handle it (mark_end at '/')
    } else if (lexer->lookahead == '#' && found_line_end) {
      // A `#` at line start is ALWAYS a full-line comment, unconditionally
      // (faithful to otmlparser `parseLine`: `line.starts_with("#")`). Like
      // `//`, it is indentation-neutral: mark the structural token's end at the
      // `#` (zero-width) and let the next real line decide the block structure;
      // the COMMENT token is emitted on the following scan. `found_line_end`
      // gates this so a mid-line `#` (e.g. a `&tag:` hash literal) stays data.
      lexer->mark_end(lexer);
      advance(lexer);
      indent = peek_next_real_indent(lexer);
      break; // emit the structural token here; COMMENT follows next scan
    } else {
      lexer->mark_end(lexer);
      break; // real content
    }
  }

  if (!found_line_end) {
    return false;
  }

  uint16_t cur = top(s);

  if (valid_symbols[INDENT] && indent > cur) {
    if (!push(s, (uint16_t)indent)) {
      return false; // out of memory: fail safe into error recovery
    }
    lexer->result_symbol = INDENT;
    return true;
  }

  if (indent < cur) {
    // Count how many levels to close.
    uint16_t levels = 0;
    for (uint32_t i = s->len; i > 1; i--) {
      if (s->data[i - 1] > indent) {
        levels++;
      } else {
        break;
      }
    }
    if (valid_symbols[NEWLINE]) {
      // Terminate the current statement now; queue the dedents for next calls.
      if (levels > 0) {
        s->queued_dedents = levels;
      }
      lexer->result_symbol = NEWLINE;
      return true;
    }
    if (valid_symbols[DEDENT] && s->len > 1) {
      s->len--;
      lexer->result_symbol = DEDENT;
      return true;
    }
    return false;
  }

  if (valid_symbols[NEWLINE]) {
    lexer->result_symbol = NEWLINE;
    return true;
  }

  return false;
}

unsigned tree_sitter_otui_external_scanner_serialize(void *payload,
                                                     char *buffer) {
  Scanner *s = (Scanner *)payload;
  unsigned size = 0;
  buffer[size++] = (char)s->queued_dedents;
  uint32_t count = s->len;
  if (count > (TREE_SITTER_SERIALIZATION_BUFFER_SIZE - 1) / 2) {
    count = (TREE_SITTER_SERIALIZATION_BUFFER_SIZE - 1) / 2;
  }
  for (uint32_t i = 0; i < count; i++) {
    buffer[size++] = (char)(s->data[i] & 0xff);
    buffer[size++] = (char)((s->data[i] >> 8) & 0xff);
  }
  return size;
}

void tree_sitter_otui_external_scanner_deserialize(void *payload,
                                                   const char *buffer,
                                                   unsigned length) {
  Scanner *s = (Scanner *)payload;
  s->len = 0;
  s->queued_dedents = 0;
  if (length == 0) {
    push(s, 0); // base indentation level (best effort; top() tolerates len 0)
    return;
  }
  unsigned pos = 0;
  s->queued_dedents = (uint16_t)(unsigned char)buffer[pos++];
  while (pos + 1 < length) {
    uint16_t lo = (unsigned char)buffer[pos++];
    uint16_t hi = (unsigned char)buffer[pos++];
    push(s, (uint16_t)(lo | (hi << 8)));
  }
}

void *tree_sitter_otui_external_scanner_create(void) {
  Scanner *s = (Scanner *)calloc(1, sizeof(Scanner));
  if (s != NULL) {
    push(s, 0); // base indentation level (top() tolerates an empty stack)
  }
  return s;
}

void tree_sitter_otui_external_scanner_destroy(void *payload) {
  Scanner *s = (Scanner *)payload;
  if (s) {
    free(s->data);
    free(s);
  }
}
