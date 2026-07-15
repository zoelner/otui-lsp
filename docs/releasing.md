# Releasing

`otui-lsp` ships as pre-built binaries attached to a [GitHub Release]. The VS Code extension
([`otui-vscode-extension`]) consumes those binaries, so the **asset-name contract** below is the
integration point between this repository's CI and the extension's — keep it stable.

[GitHub Release]: https://github.com/zoelner/otui-lsp/releases
[`otui-vscode-extension`]: https://github.com/zoelner/otui-vscode-extension

## Cutting a release

1. Bump `[workspace.package] version` in `Cargo.toml` to `X.Y.Z`, then run `cargo build` once so
   `Cargo.lock` picks up the new version. Commit both.
2. Tag the commit `vX.Y.Z` and push the tag:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

3. Pushing the tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml): it
   builds every target, then creates the GitHub Release and uploads the assets. A guard fails the run
   if the tag's version does not match `Cargo.toml`. A tag containing a hyphen (`v0.2.0-rc.1`) is
   published as a **pre-release**.

To rehearse without publishing, run the workflow via **Actions → Release → Run workflow**
(`workflow_dispatch`): it runs the build matrix only and creates no Release.

## Asset-name contract

Each release carries one gzipped binary per platform plus its `.sha256`. The name encodes the Rust
target triple; `gunzip` restores a runnable binary:

| Asset (`.gz`) | Rust target | VS Code `--target` |
|---|---|---|
| `otui-lsp-x86_64-unknown-linux-gnu.gz` | `x86_64-unknown-linux-gnu` | `linux-x64` |
| `otui-lsp-aarch64-unknown-linux-gnu.gz` | `aarch64-unknown-linux-gnu` | `linux-arm64` |
| `otui-lsp-x86_64-apple-darwin.gz` | `x86_64-apple-darwin` | `darwin-x64` |
| `otui-lsp-aarch64-apple-darwin.gz` | `aarch64-apple-darwin` | `darwin-arm64` |
| `otui-lsp-x86_64-pc-windows-msvc.exe.gz` | `x86_64-pc-windows-msvc` | `win32-x64` |

## Consuming the binaries from the extension

The extension repository owns this side; both approaches below rely only on the contract above.

### Recommended — download at runtime

Keep a single, platform-agnostic VSIX. On activation, the extension resolves the server binary:

1. If the user set `otui.server.path`, use it (power-user override).
2. Otherwise pick the asset for `process.platform` + `process.arch` (map to the triple via the table
   above), look for it cached under the extension's `globalStorageUri` keyed by server version, and if
   missing download it from the release of the **pinned** server tag, verify its `.sha256`, `gunzip`
   it, and `chmod +x` on unix.
3. Spawn the resulting binary as the language server over stdio.

Pin the server version the extension expects (e.g. a `SERVER_VERSION = 'vX.Y.Z'` constant); bumping
that constant is how the extension adopts a newer server.

### Alternative — bundle a per-platform VSIX (offline install)

For an install that needs no network, publish one VSIX per platform. In the extension's release CI,
run a matrix over the five VS Code targets; for each, download the matching asset, place the binary
inside the package, then:

```bash
gh release download vX.Y.Z --repo zoelner/otui-lsp --pattern 'otui-lsp-<triple>*.gz'
# gunzip into e.g. server/otui-lsp[.exe], then:
npx @vscode/vsce publish --target <vscode-target> -p "$VSCE_PAT"
# optional Open VSX mirror:
npx ovsx publish --target <vscode-target> -p "$OVSX_PAT"
```

This is purely additive over the runtime-download approach — same release, same asset names.
