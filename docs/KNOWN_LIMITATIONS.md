# Known limitations (deferred findings)

Surfaced during a module-by-module review + simplification pass (Codex-verified).
These are real but were deferred because a correct fix needs a design decision,
external-tool validation, or a seam/API change beyond a local edit. The common
case works today; each note records the edge and why it was left.

## Package managers (uv / npm)
- **`uv tool upgrade` relies on uv retaining install-time constraints.** A pinned
  PyPI tool (`version`/`extras`/`with`) stays converged only because `uv tool
  upgrade` honors the recorded requirement. If that assumption ever breaks, switch
  pinned upgrades to `uv tool install --reinstall` using `uv::install_args`.
  (`src/sources/uv.rs::upgrade`)
- **`install_paths` use the package name, not the real binary name.** For scoped
  or renamed-bin packages (`awscli`→`aws`, `@scope/pkg`), the recorded install
  path can be wrong. Removal is UNAFFECTED (it shells out to
  `uv tool uninstall` / `npm rm -g <pkg>`); only PATH/info display is imperfect.
  A fix needs parsing tool output / inspecting `UV_TOOL_BIN_DIR` symlinks.
  (`src/sources/uv.rs`, `src/sources/npm.rs`)

## aqua synthesis
- **Per-platform in-archive member names can't be represented.** `synth` emits a
  single scalar `exe`, but `files[].src` (`.AssetWithoutExt`) varies per platform.
  The current `exe = fname` works for aqua's single-file (`.zst`) assets (ubi
  ignores `exe` on the non-archive decompression path). Tools whose in-archive
  member name differs AND varies per platform need per-platform `exe` support, or
  a "bail when not representable" guard — validate against real ubi behavior first.
  (`src/aqua/synth.rs`)

## `outdated` / version discovery
- **Go latest-version query uses the install package path, not the module root.**
  `go:golang.org/x/tools/cmd/stringer` installs fine, but the `@latest` query 404s
  because the module root is `golang.org/x/tools`. Affects only `outdated`/`upgrade`
  version comparison, not install. A correct fix needs Go module-root resolution
  (network-dependent; a naive parent-walk adds round-trips). (`src/outdated.rs`)

## `add` / auth
- **`add --force` overwrites the state record without uninstalling old files.** If
  `rename`, `exes`, or the source changed, old tracked binaries can leak. A correct
  fix is source-aware (uv/cargo/npm are tool-managed — naive unlink is wrong), i.e.
  remove-the-old-record-then-add semantics. (`src/cli.rs::persist_and_install`)
- **GitHub/GitLab latest queries are not token-aware.** Installs use
  `UBIX_GITHUB_TOKEN`/`UBIX_GITLAB_TOKEN`, but the latest-version/aqua-discovery
  queries don't, so private or rate-limited repos install yet fail to compare/record
  latest. The `HttpClient` seam only supports URL-only GETs; adding per-request auth
  is a trait/API change. (`src/outdated.rs`, `src/http.rs`)
