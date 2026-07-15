# Architecture

`otui-lsp` is a Cargo workspace built around one rule: **language semantics are kept separate from
the LSP transport.** All the analysis lives in a pure library; a thin server wraps it in the
protocol, and the same library backs the command-line tools.

## Workspace layout

| Crate | Role |
|---|---|
| `tree-sitter-otui` | The tree-sitter grammar — including an external C scanner for OTUI's significant indentation — plus the highlight/injection queries. |
| `otui-core` | The **pure** language library: parsing, diagnostics, symbols, completion, hover and formatting. No `lsp-types`, no I/O; all spans are byte offsets, so it is protocol-agnostic. |
| `lang-api` | A `LanguageService` trait seam and shared value types (`Diagnostic`, `Severity`, `ByteSpan`). It lets a second language (the planned OTClient HTML/CSS UI) be added without rewriting the server. |
| `otui-lsp-server` | The `otui-lsp` binary. A synchronous `lsp-server` (the same transport crate rust-analyzer uses) + `lsp-types` shell over `otui-core`. It owns **all** I/O and the workspace index, and also carries the CLI (`fmt` / `check`). |
| `xtask` | Dev tooling — generates the per-fork property/color catalog from the engine source, and runs the corpus census. |

## Why the split

The core being pure is what makes the project trustworthy:

- **One analysis, two front-ends.** The editor (over LSP) and the CLI (`otui-lsp check`, used in CI)
  run the *same* `otui-core` analysis over the *same* workspace index. Their diagnostics agree for
  the same on-disk corpus — CI cannot flag something the editor stays silent about, or vice versa.
  (Unsaved editor buffers are authoritative in the editor, so they may differ from CI until saved.)
- **The transport is replaceable.** Because `otui-core` never mentions `lsp-types`, the protocol
  shell is a shallow layer. Nothing about the language logic is coupled to a particular editor
  protocol or lsp-types version.
- **Fidelity is testable in isolation.** Diagnostics and formatting are plain functions over text and
  byte-offset spans, so they can be measured directly against the real OTClient `.otui` corpus
  without standing up a server.

## The format contract

The exact OTUI/OTML behavior this server implements — tolerances, strictnesses, and the closed value
sets — is vendored at [`docs/otui-language-service-spec.md`](docs/otui-language-service-spec.md).
