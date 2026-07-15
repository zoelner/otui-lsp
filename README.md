# otui-lsp

A [Language Server](https://microsoft.github.io/language-server-protocol/) for **OTUI/OTML** —
the UI markup language of the [OTClient](https://github.com/mehah/otclient) game client.

`otui-lsp` gives any LSP-capable editor (VS Code, Neovim, and custom editors) real language
intelligence for `.otui` / `.otmod` / `.otfont` files — diagnostics, completion, hover,
go-to-definition, find-references, formatting and more — with behavior that faithfully mirrors the
real OTClient engine. The **same binary** doubles as a one-shot CLI formatter and linter for CI.

> **Status:** in active development, preparing its first public release (`0.1.0`). This repository
> ships **only the language server**. Editor clients (a VS Code extension, a Neovim plugin, …) live in
> separate repositories and talk to this server over LSP.

## Why

OTUI is an indentation-based markup (2 spaces per level, `Name < Base` inheritance, anchors,
`$state` selectors, `@`/`&`/`!` Lua-bearing tags). Editors today treat it as plain YAML. This
server understands the format the way the engine does — including its exact tolerances (an unknown
property is *silently ignored*, so it's a hint, not an error) and strictnesses (a tab in the
indentation is a hard error).

## Quick start

```bash
git clone https://github.com/zoelner/otui-lsp && cd otui-lsp
cargo build --release          # → target/release/otui-lsp  (an stdio LSP binary that is also the CLI)
```

Then pick a path:

- **In an editor** — wire the binary into your editor's LSP client and open your module folder. See
  [Using it in an editor](#using-it-in-an-editor).
- **In CI** — lint a project without speaking LSP at all:

  ```bash
  target/release/otui-lsp check path/to/modules
  ```

  See [Command-line (CI)](#command-line-ci).

Open the project **folder** (not a lone file) so the server can index the whole workspace.

## Features

Working today, all resolving **workspace-wide** (the server indexes `.otui`, `.otmod` and `.otfont`
files across the workspace — plus the `.lua` modules that declare widget classes — not just the open
ones):

- **Diagnostics** — tab/odd indentation and invalid depth jumps, syntax errors, unknown base/root
  styles (`Name < Base`), unknown properties (a *hint* — the engine silently ignores them), invalid
  `$state`, invalid anchor edges and anchors without an anchor layout, invalid
  `display`/`layout`/`border` values, properties placed after child widgets, missing asset files
  (`image-source`, icons), and manifest checks for `.otmod`/`.otfont` (missing roots, unknown keys).
- **Completion** — properties, `$state` names, anchor edges/targets, `@event` names.
- **Hover**, **go-to-definition**, **type definition** (instance → its style), **implementation**
  (style → its subtypes), **type hierarchy** (the `Name < Base` graph), **find references**,
  **rename**, **document & workspace symbols**, **document highlight**.
- **Semantic highlighting**, **color swatches** (`documentColor`), **clickable asset links**
  (`documentLink` on `image-source` etc.), **code actions** (tab→spaces, "did you mean" fixes),
  **code lens** ("N widgets inherit this style"), **inlay hints** (the resolved native `UI*` ancestor
  of a based style), **formatting** (whole document, range, and on-type auto-indent), **folding**.

**Planned, not yet built:** the OTClient HTML/CSS UI (behind the `lang-api` seam) and semantic
intelligence *inside* embedded Lua bodies (embedded-Lua highlighting already works).

## Using it in an editor

The release build produces `target/release/otui-lsp`, an stdio LSP binary. Point an editor's LSP
client at it for `.otui` / `.otmod` / `.otfont` files.

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

## Command-line (CI)

The same binary also runs as a one-shot CLI, useful in CI without speaking LSP at all:

```bash
otui-lsp fmt <paths...> [--check|--write]                       # format .otui files (default: --check)
otui-lsp check <paths...> [--deny error|warnings|hints] [--format human|sarif]
                                                                  # lint .otui/.otmod/.otfont + asset refs
```

`check` builds the same widget-aware workspace index the language server uses (so a widget class
declared only in a `.lua` module is still recognized), then exits non-zero when a finding at or
above `--deny`'s severity (`error` by default) is present.

`--format sarif` prints a single [SARIF 2.1.0](https://json.schemastore.org/sarif-2.1.0.json) log
on stdout instead of the default rustc-style lines — useful for GitHub code-scanning annotations on
a pull request:

```yaml
- name: otui-lsp check
  run: otui-lsp check --format sarif . > otui.sarif

- uses: github/codeql-action/upload-sarif@v3
  with:
    sarif_file: otui.sarif
```

The exit code is unaffected by `--format`: it is still governed entirely by `--deny`, so pair the
`check` step above with `continue-on-error: true` if you want the SARIF upload to run (and annotate
the PR) even when the lint step itself would otherwise fail the job.

## Architecture

A Cargo workspace that keeps language semantics (`otui-core`, a pure and protocol-agnostic library)
separate from the LSP transport (`otui-lsp-server`), so the editor and the CLI agree on the same
on-disk corpus. See [ARCHITECTURE.md](ARCHITECTURE.md) for the crate layout and the design rationale.

## Building

`rust-toolchain.toml` pins the compiler, and rustup installs it on first use — CI and local
development are held to the same rustc and the same clippy, so a lint cannot pass in one and fail in
the other.

```bash
./ci.sh          # fmt + clippy + tests (the single quality gate)
./ci.sh --quick  # fmt + clippy only
```

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
