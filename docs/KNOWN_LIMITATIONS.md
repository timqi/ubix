# Known limitations (deferred findings)

Surfaced during a module-by-module review + simplification pass (Codex-verified).
These are real but were deferred because a correct fix needs a design decision,
external-tool validation, or a seam/API change beyond a local edit. The common
case works today; each note records the edge and why it was left.

## Package managers (uv / npm / pixi)
- **pixi tracked `install_paths` assume the trampoline is named after the package.**
  `pixi global install` names its `$PIXI_HOME/bin` trampolines after the package's
  ENTRY POINTS, not the package (e.g. `ripgrep` → `rg`, `python-dotenv` → `dotenv`).
  ubix records `$PIXI_HOME/bin/<pkg>`, so `ubix info`/`list` may show a path that
  doesn't exist when they differ. Removal is unaffected (tool-managed via
  `pixi global uninstall <pkg>`). A correct fix needs to parse `pixi global list`
  output or diff the bin dir around install. (`src/sources/pixi.rs`)
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
- **aqua alias files are dropped, not installed.** ~15 of 2244 registry packages
  declare extra names for the SAME archive member (`files[].src` pointing at a
  sibling entry, e.g. claude-squad's `cs`, flyctl's `fly`, and 9 `kubectl-*`
  plugin names). aqua materializes these as links; ubix's `github:` source can't,
  so `synth` dedups them (with a step note) and installs only the real member.
  Mostly cosmetic, but two cases lose function: flyctl's PRIMARY command is the
  `fly` alias, and `kubectl <plugin>` dispatch requires the `kubectl-*` name.
  Supporting them needs an `aliases` config field + post-install symlinks tracked
  in state (removal is already unlink-by-tracked-file). (`src/aqua/synth.rs`)
- **`select_branch` hoists the unconditional `"true"` branch.** aqua evaluates
  `version_overrides` in declaration order and takes the FIRST whose constraint
  holds; ubix takes the `"true"` branch — always the last one in the registry —
  before evaluating any earlier branch. It only diverges when installing an OLD
  version of a package whose earlier branches would match: `golang/tools/godoc`,
  `golang/tools/guru`, `theupdateframework/go-tuf/{tuf,tuf-client}` and
  `oxc-project/oxc/oxlint`. Ordering it aqua's way requires the next bullet first,
  since a misparsed constraint would then silently win.
  (`src/aqua/resolve.rs::select_branch`)
- **`eval_constraint` reads only the first comparison of an expression.**
  `semver("> 1.0.0, <= 2.0.0")` is evaluated as `> 1.0.0`, and
  `semver(...) or semver(...)` as its left operand, so ~40 packages can accept a
  branch aqua would rule out (or the reverse). Fixing it means implementing
  aqua's expr grammar, not just its `semver()` helper.
  (`src/aqua/resolve.rs::eval_constraint`)
- **No base-entry fallback when every branch is ruled out.** aqua returns the base
  entry it just rejected; ubix bails with an error instead of synthesizing from a
  version the registry says the entry does not describe.
  (`src/aqua/resolve.rs::select_branch`)
- **An override's `variants:` are not evaluated.** aqua gates a platform override
  on runtime variant keys (today only `libc`, glibc vs musl) and skips an override
  whose variants don't match. ubix doesn't detect libc, so it treats a
  variants-bearing override as matching — which lands on the glibc entry that
  registries list first (`anthropics/claude-code`, `just`, 3 others; 28 lines
  registry-wide). On musl that picks the glibc asset. Detecting libc is a
  `platform.rs` change plus a runtime probe. (`src/aqua/resolve.rs::pick_override`)

## bare-name discovery (`add <name>` / `which`)
- **Branch selection is approximated, not evaluated.** The scanner can't run
  aqua's `version_constraint` expressions. It reads `version_overrides` only where
  the package-level constraint is the literal `"false"` (1693 packages) — the one
  value that can never hold, and so the only one that hands the package to its
  branches — and there it takes the unconditional `"true"` branch, which is what
  `resolve::select_branch` installs from. Every other package-level constraint is
  a version guard whose truth depends on the version being installed, so the base
  entry is reported (which is also what aqua falls back to when no branch
  matches). For the ~88 packages whose every branch is constrained, the scanner
  reports the package-level `files[]` rather than guessing a branch — guessing the
  last-listed one made `dineshba/tf-summarize` claim `terraform-plan-summarize`, a
  command it no longer ships. (`src/aqua/registry.rs`, `src/aqua/resolve.rs`)
- **Command evidence is scoped to the running host.** A platform `overrides[]`
  entry counts only when its `goos`/`goarch`/`envs:` match this machine, and only
  the FIRST matching entry is applied — mirroring `resolve::effective_for`. So the
  same registry gives different answers on different hosts
  (`ImageMagick/ImageMagick` installs `magick` on linux and eight commands on
  windows), and a package that names its commands only for platforms you are not
  on falls back to the package-level `files[]` — or, failing that, to the repo
  name. A YAML alias (`files: *anchor`) is not resolved by a line scan; the scope
  inherits instead — currently unreachable from a `"true"` branch or a base entry.
  `variants:` are not evaluated here either (see above). (`src/aqua/registry.rs`)
- **A package with no build for this host is reported, not hidden.** When
  `supported_envs` exclude the host or the entry ubix would install from is
  `no_asset`, the candidate is marked unavailable: it still appears in
  `ubix which` (so a linux `ubix which xcodes` explains itself rather than saying
  "not found"), ranks below anything installable, and never auto-picks — `add`
  fails with `no linux/amd64 build`. 37 of 2277 packages are in that state on
  linux/amd64. What it can NOT see is a package that is unavailable only because
  no asset template resolves for the host; that needs the full synthesis path.
  (`src/aqua/registry.rs`, `src/discover.rs`)
- **A mirror that declares the same command ties with upstream.** Matching now
  follows the commands a package is known to INSTALL, so `ubix which rg` lists
  `BurntSushi/ripgrep` first — but `microsoft/ripgrep-prebuilt` declares `rg` too,
  and an exact tie is never auto-installed. `ubix add rg` therefore asks; pick
  with `--pick 1` or name the source. (`src/discover.rs`)
- **A command a package stopped shipping still surfaces as a weak hit.** Exact
  matching uses the effective command set, but the SUBSTRING pass still scans the
  advertised `files[]`, so `ubix which cfs-preload` lists `aqua:cubefs/cubefs`
  under `name contains` with an honest `installs cfs-cli, …` note. It is offered,
  never auto-installed. (`src/discover.rs`, `src/aqua/registry.rs`)
- **Only aqua is searched.** A tool that exists solely on PyPI, npm, or
  conda-forge doesn't resolve from a bare name; use the explicit prefix (or
  `ubix search --pixi`). Probing those registries per query is deferred because
  each is a separate network round-trip on the `add` hot path. (`src/discover.rs`)

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
