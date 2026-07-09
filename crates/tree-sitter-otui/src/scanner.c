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
#include <stdlib.h>
#include <string.h>

enum TokenType {
  NEWLINE,
  INDENT,
  DEDENT,
  BLOCK_SCALAR_CONTENT,
  ERROR_SENTINEL,
};

typedef struct {
  uint32_t len;
  uint32_t cap;
  uint16_t *data;
  uint16_t queued_dedents;
} Scanner;

static inline void push(Scanner *s, uint16_t v) {
  if (s->len == s->cap) {
    s->cap = s->cap ? s->cap * 2 : 8;
    s->data = realloc(s->data, s->cap * sizeof(uint16_t));
  }
  s->data[s->len++] = v;
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
      advance(lexer);
      if (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
        continue; // another `# ` comment: skip
      }
      return indent; // `#Name` freeze header: real content
    }
    return indent; // real content
  }
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
    } else if (lexer->lookahead == '/') {
      // Possible `//` comment line. Mark the structural token's end at the `/`
      // (zero-width) so the comment's bytes are handed back to the internal
      // lexer, which tokenizes it as an `extras` `comment`. The block the
      // comment sits in is decided by the next real line, not the comment's own
      // column, keeping comments indentation-neutral.
      lexer->mark_end(lexer);
      advance(lexer);
      if (lexer->lookahead == '/') {
        indent = peek_next_real_indent(lexer);
        break; // emit the structural token here; internal lexer gets the comment
      }
      break; // a lone '/', let the internal lexer handle it (mark_end at '/')
    } else if (lexer->lookahead == '#') {
      // Distinguish a `#` full-line comment (§2.1: indentation-neutral, like
      // `//`) from a `#Name < Base` freeze header (real content). They differ
      // only by the char after `#`, so peek past it. `mark_end` is set to the
      // position of `#` *before* advancing, so both paths hand the `#` back to
      // the internal lexer un-consumed.
      lexer->mark_end(lexer);
      advance(lexer);
      if (lexer->lookahead == ' ' || lexer->lookahead == '\t') {
        indent = peek_next_real_indent(lexer);
        break; // emit the structural token here; internal lexer gets the comment
      }
      break; // `#Name` freeze header — re-lexed from the marked end
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
    push(s, (uint16_t)indent);
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
    push(s, 0); // base indentation level
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
  push(s, 0); // base indentation level
  return s;
}

void tree_sitter_otui_external_scanner_destroy(void *payload) {
  Scanner *s = (Scanner *)payload;
  if (s) {
    free(s->data);
    free(s);
  }
}
