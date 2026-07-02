# CLAUDE.md

Guidance for agents working in this repo. Keep it an index — high-level
principles and pointers, not a changelog.

## What ubix is

A declarative binary/CLI installer & tracker. Users declare tools in
`config.toml`; ubix installs/upgrades/removes them across many sources and
records results in `state.toml`. Wraps `ubi` for GitHub/GitLab release assets;
has its own handlers for pypi(uv), npm(fnm), cargo, go, url, and templated URLs.
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
  parsing (`parse_spec`), the `Source` trait, and `InstallOutcome`. `template.rs`
  is the templated-URL source (`http:` is a back-compat alias).
- `engine.rs` — `ReleaseEngine` trait + `UbiEngine` (drives ubi on a
  current-thread tokio runtime) + `atomic_install` / `sha256_*`.
- `aqua/` — aqua-registry integration as a **config generator** (NOT a runtime
  source): fetch registry.yaml → resolve branch/platform → synthesize a
  `github:` `ToolConfig`. `prune.rs` simulates ubi's asset picker.
- `outdated.rs` — latest-version queries (pure parsers + `HttpClient` dispatch).
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
- **uv/cargo/npm removal is tool-managed** (`uv tool uninstall`, etc.); only
  github/gitlab/url/go removal unlinks tracked files directly.

## Working style

- Prefer small, source-local edits with a regression test; keep `cargo test` and
  `cargo clippy --all-targets` green.
- Behavior changes to external-tool interaction (uv/fnm/go/ubi) can't be fully
  validated offline — propose/verify before landing rather than guessing.
- Known deferred edge cases live in `docs/KNOWN_LIMITATIONS.md`; check it before
  "fixing" something that may be intentional.
