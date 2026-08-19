# CLAUDE.md

Guidance for agents working in this repo. Keep it an index — high-level
principles and pointers, not a changelog.

## What ubix is

A declarative binary/CLI installer & tracker. Users declare tools in
`config.toml`; ubix installs/upgrades/removes them across many sources and
records results in `state.toml`. Wraps `ubi` for GitHub/GitLab release assets;
has its own handlers for pypi(uv), npm(fnm), cargo, go, pixi(conda), url, and
templated URLs.
Full spec: `docs/PRD.md`. Aqua generator: `docs/aqua-plan.md`.

## Build / test / lint

```sh
cargo build            # or: just install  (release + ~/.local/bin)
cargo test             # ~290 unit tests, all offline (no network)
cargo clippy --all-targets   # must stay warning-clean
```

Version metadata (`UBIX_VERSION` / `UBIX_GIT_SHA` / `UBIX_COMMIT_DATE`) is
injected by `build.rs`. Release profile optimizes for size (`opt-level=z`,
lto, strip, panic=abort).

## Architecture (module map)

- `main.rs` — entry, logger, verbosity.
- `cli.rs` — clap definitions + `App` command dispatch (the orchestration layer;
  by far the largest file). Owns install/upgrade/remove/list/info/edit/doctor/
  bootstrap/sources/search flows and `decide_action` (the upgrade state machine).
- `config.rs` / `state.rs` — `config.toml` / `state.toml` models. `state` holds
  the `LockedState` flock guard.
- `sources/` — one module per source. `mod.rs` defines `SourceKind`, spec
  parsing (`parse_spec`), the `Source` trait, and `InstallOutcome`. The `url`
  source is unified: `url.rs::install` IS the general template flow (resolve
  version → render → download → extract); a fixed URL is just the degenerate
  template (no placeholders → version "url" sentinel, no render, plus sidecar
  checksum discovery). `template.rs` holds the render/version-resolution helpers
  it calls (`is_templated`/`resolve_version`/`render_url`/`latest`), NOT an
  install path. `template:`/`http:` are back-compat spec aliases for `url:`.
- `engine.rs` — `ReleaseEngine` trait + `UbiEngine` (drives ubi on a
  current-thread tokio runtime) + `atomic_install` / `sha256_*`.
- `aqua/` — aqua-registry integration as a **config generator** (NOT a runtime
  source): fetch registry.yaml → resolve branch/platform → synthesize a
  `github:` `ToolConfig`. `prune.rs` simulates ubi's asset picker.
- `outdated.rs` — latest-version queries (pure parsers + `HttpClient` dispatch).
- `prefix_dev.rs` — prefix.dev GraphQL client (conda latest-version + package
  search) for the `pixi` source; pure query builders/parsers + POST dispatch.
- `bootstrap.rs` — rust/go toolchain fetches (python/nodejs handled in `cli.rs`).
- `archive.rs`, `checksum.rs`, `paths.rs`, `platform.rs`, `progress.rs`,
  `runner.rs`, `http.rs` — utilities/seams.

## Seams (test without network/subprocess)

Everything external goes through a trait so unit tests inject fakes:

- `CommandRunner` (`runner.rs`) → `SystemRunner` / `MockRunner`.
- `HttpClient` (`http.rs`) → `ReqwestClient` / `MockHttp`.
- `ReleaseEngine` (`engine.rs`) → `UbiEngine` / test `FakeEngine`.

New code that shells out or fetches MUST go through these seams and ship a
fixture-based test. All tests are offline.

## Invariants — don't regress these

- **Atomic install (§8.7).** Stage into a tempdir, verify, then atomically
  rename into `install_dir`. State is written ONLY after a successful install.
  Multi-exe: validate every entry before publishing any (`plan_exe_installs`).
- **State lock (§8.6).** The flock lives on a stable `state.toml.lock` sibling,
  not on `state.toml` (which `save()` renames). Keep it that way.
- **Forward-compat config/state.** All fields are `#[serde(default)]`; additive
  optional fields keep `schema_version = 1` so older ubix ignores unknowns. Do
  NOT add `#[serde(deny_unknown_fields)]` — it breaks this contract.
- **`matching` semantics.** ubi `.matching()` is a case-sensitive substring, and
  synthesized per-platform maps use `""` as the "no filter → let ubi decide"
  sentinel (`PlatformString::resolve` maps `""`→`None`). Never DROP a platform
  key from a partial matching map — `resolve` errors on a missing key.
- **npm goes through `fnm exec --using=default -- npm …`** — never bare `npm`.
- **uv/cargo/npm/pixi removal is tool-managed** (`uv tool uninstall`,
  `pixi global uninstall`, etc.); only github/gitlab/url/go removal unlinks
  tracked files directly.
- **pixi has no bin-dir redirect** (unlike uv's `UV_TOOL_BIN_DIR`): binaries land
  as trampolines in `$PIXI_HOME/bin` (default `~/.pixi/bin`), which is what we
  track — NOT `install_dir`. pixi locators are conda MatchSpecs and may be
  channel-qualified (`pixi:bioconda::samtools`; bare name → conda-forge).
- **pixi metadata comes from the prefix.dev GraphQL API** (`prefix_dev.rs`, POST
  via the `HttpClient` seam): `latest_version` powers `outdated`/`upgrade`;
  `search` powers `ubix search --pixi` across ALL prefix.dev channels. conda has
  no per-package REST endpoint — do not reach for anaconda.org.

## Working style

- Prefer small, source-local edits with a regression test; keep `cargo test` and
  `cargo clippy --all-targets` green.
- Behavior changes to external-tool interaction (uv/fnm/go/ubi) can't be fully
  validated offline — propose/verify before landing rather than guessing.
- Known deferred edge cases live in `docs/KNOWN_LIMITATIONS.md`; check it before
  "fixing" something that may be intentional.
