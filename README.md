# otui-lsp

A [Language Server](https://microsoft.github.io/language-server-protocol/) for **OTUI/OTML** —
the UI markup language of the [OTClient](https://github.com/mehah/otclient) game client.

`otui-lsp` gives any LSP-capable editor (VS Code, Neovim, and custom editors) real language
intelligence for `.otui` / `.otmod` / `.otfont` files: syntax highlighting, diagnostics,
completion, hover, go-to-definition, find-references, document/workspace symbols, formatting and
more — with behavior that faithfully mirrors the real OTClient engine.

> **Status:** early, under active development. This repository ships **only the language
> server**. Editor clients (a VS Code extension, a Neovim plugin, …) live in separate
> repositories and talk to this server over LSP.

## Why

OTUI is an indentation-based markup (2 spaces per level, `Name < Base` inheritance, anchors,
`$state` selectors, `@`/`&`/`!` Lua-bearing tags). Editors today treat it as plain YAML. This
server understands the format the way the engine does — including its exact tolerances (an
unknown property is *silently ignored*, so it's a hint, not an error) and strictnesses (a tab in
the indentation is a hard error).

## Architecture

A Cargo workspace that keeps language semantics separate from the LSP transport:

| Crate | Role |
|---|---|
| `tree-sitter-otui` | tree-sitter grammar (external scanner for indentation) + highlight/injection queries |
| `otui-core` | pure language library — parsing, diagnostics, symbols, completion, formatting (byte-offset, protocol-agnostic) |
| `lang-api` | a `LanguageService` trait seam so a future HTML/CSS language can be added without rewriting the server |
| `otui-lsp-server` | the LSP server binary — a synchronous `lsp-server` (the rust-analyzer transport crate) + `lsp-types` shell over `otui-core` |
| `xtask` | dev tooling — generates the per-fork property/color catalog from the engine source |

The format contract this server implements is vendored at
[`docs/otui-language-service-spec.md`](docs/otui-language-service-spec.md).

## Features

Working today, all resolving **workspace-wide** (the server indexes `.otui` files across the
workspace, not just the open ones):

- **Diagnostics** — tab-in-indentation errors, invalid depth jumps, unknown-property hints, unknown
  `$state`, invalid anchor edges, invalid `display`/`layout`/`border` values.
- **Completion** — properties, `$state` names, anchor edges/targets, `@event` names.
- **Hover**, **go-to-definition**, **type definition** (instance → its style), **implementation**
  (style → its subtypes), **type hierarchy** (the `Name < Base` graph), **find references**,
  **rename**, **document & workspace symbols**, **document highlight**.
- **Semantic highlighting**, **color swatches** (`documentColor`), **clickable asset links**
  (`documentLink` on `image-source` etc.), **code actions** (tab→spaces, "did you mean" fixes),
  **formatting** (whole document and range), **folding**.

**Planned, not yet built:** the OTClient HTML/CSS UI (behind the `lang-api` seam) and semantic
intelligence *inside* embedded Lua bodies (embedded-Lua highlighting already works).

## Using it in an editor

Build the server (`cargo build --release` → `target/release/otui-lsp`, an stdio LSP binary), then
point an editor's LSP client at it for `.otui` / `.otmod` / `.otfont` files.

- **VS Code** — install the companion [`otui-vscode-extension`](https://github.com/zoelner/otui-vscode-extension)
  (a thin client; set `otui.server.path` to your built binary).
- **Neovim** — register the filetype and start the server (no plugin needed):

  ```lua
  vim.filetype.add({ extension = { otui = "otui", otmod = "otui", otfont = "otui" } })
  vim.api.nvim_create_autocmd("FileType", {
    pattern = "otui",
    callback = function(args)
      vim.lsp.start({
        name = "otui-lsp",
        cmd = { vim.fn.expand("~/path/to/otui-lsp/target/release/otui-lsp") },
        root_dir = vim.fs.root(args.buf, { ".git", ".otmod", "modules", "data" }) or vim.fn.getcwd(),
      })
    end,
  })
  ```

Open the project **folder** (not a single file) so the server can index the whole workspace.

## Building

Requires a stable Rust toolchain.

```bash
./ci.sh          # fmt + clippy + tests (the single quality gate)
./ci.sh --quick  # fmt + clippy only
```

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
