# ubix

Declarative binary/CLI tool installer & tracker — one `config.toml` describing the
tools you want, installed and kept up to date from many sources. Built on
[`ubi`](https://crates.io/crates/ubi) for GitHub/GitLab release assets, plus
first-class handlers for PyPI, npm, cargo, go, direct URLs, and templated URLs.

## Why

Instead of a pile of `curl | sh` snippets and manual `cargo install` lines, you
declare tools once:

```toml
# ~/.config/ubix/config.toml
[settings]
install_dir = "~/.local/bin"

[tools.eza]
spec = "github:eza-community/eza"

[tools.ruff]
spec = "pypi:ruff"

[tools.gh]
spec = "github:cli/cli"
```

…then `ubix upgrade --all` installs the missing ones, upgrades the rest, and
records exactly what landed where in a tracked state file.

## Install

```sh
just install          # cargo build --release + install to ~/.local/bin
# or
cargo build --release && install -m0755 target/release/ubix ~/.local/bin/
```

Make sure `~/.local/bin` is on your `PATH` (`ubix doctor` checks this).

## Sources

| Prefix | Backend | Example |
|---|---|---|
| `github` | ubi (GitHub Releases) | `github:eza-community/eza` |
| `gitlab` | ubi (GitLab Releases; `--host` for self-hosted) | `gitlab:group/repo` |
| `url` | built-in download; fixed URL **or** templated URL (`{version}`/`{os}`/`{arch}`) | `url:https://…/x-linux.tar.gz` · `url:https://…/{version}/{os}-{arch}/bin` |
| `pypi` | `uv tool install` | `pypi:ruff` |
| `npm` | `npm -g` on fnm's default LTS node | `npm:pnpm` |
| `cargo` | `cargo install --root ~/.local` | `cargo:ripgrep` |
| `go` | `GOBIN=~/.local/bin go install` | `go:example.com/cmd/tool@latest` |
| `pixi` | `pixi global install` (conda; prefix.dev) | `pixi:ripgrep` · `pixi:bioconda::samtools` |

A bare `owner/repo` uses `settings.default_source` (default `github`).
The legacy `template:` and `http:` prefixes are kept-for-compat aliases for
`url:` (a plain URL is just a template with no placeholders).
Run `ubix sources` for the live list.

## Commands

```
ubix add <spec> [--name N] [--matching S] [--exe E] [--exes A,B] [--tag T]
                [--host U] [--version V] [--rename R] [--force]
ubix upgrade [names…] [--all] [--force] [--dry-run] [--prune] [--json]
ubix remove <names…> [--force]   # several at once; one bad name does not abort the rest
ubix list [--json]
ubix info <name | spec>        # declared tool → local info; a spec (github:…/pixi:…) → remote metadata to vet it
ubix edit                       # open config.toml in $EDITOR
ubix doctor                     # check tools + PATH readiness
ubix bootstrap <rust|go|python|nodejs|pixi> [--reinstall]
ubix sources
ubix search <query> [--add] [--name N] [--aqua|--pixi] [--channel C]
                                # searches aqua-registry (github:) AND prefix.dev (pixi:) in parallel;
                                # prints each hit's ready-to-run `ubix add` command
```

Global: `-q/--quiet`, `-v/--verbose`, `-y/--yes`, `--json`.

### Machine-readable output (`--json`)

`ubix list --json` and `ubix upgrade --json` write **exactly one JSON document**
to stdout and nothing else — progress and errors go to stderr, exit codes keep
their meaning (non-zero on failure). Every other command **rejects** `--json`
with an error instead of ignoring it. Both documents carry `schema_version`
(currently `1`) so consumers can detect a shape change.

```sh
ubix list --json | jq '.tools[] | select(.exists | not) | .name'   # tracked but gone from disk
ubix upgrade --all --json | jq '.summary'
```

`list --json` → `{schema_version, install_dir, tools[]}`; each tool carries
`name`, `spec`, `source`, `locator`, `installed`, `installed_version`,
`install_paths` (absolute), `exists` (all tracked paths present right now),
`missing_paths`, the pins `tag`/`version`, and `installed_at`/`updated_at`.

`upgrade --json` → `{schema_version, dry_run, tools[], summary}`; each tool
carries `name`, `action`, `from_version`, `to_version`, `reason` and `error`.
Actions: `installed`, `upgraded`, `skipped`, `pinned-skip`, `failed`, `pruned`,
`orphan` (reported without `--prune`), plus the `--dry-run` variants
`would-install`, `would-upgrade`, `would-prune`. `summary` is
`{total, changed, failed, by_action}` with every action name present.
Under `--json` a failing tool no longer aborts the run: it is recorded with its
`error` text, the remaining tools still run, and the process exits non-zero.

### Examples

```sh
ubix add github:eza-community/eza
ubix add pypi:ruff --version 0.6.9
ubix add github:astral-sh/uv --name uv --exes uv,uvx
ubix upgrade --all --dry-run     # preview installed vs latest
ubix upgrade --all               # install missing + upgrade
ubix upgrade --prune ruff        # also drop orphans in scope
ubix search ripgrep              # aqua + prefix.dev results side by side, each with its `ubix add` cmd
ubix search eza --aqua --add     # single exact match → install it directly
ubix search samtools --pixi --channel bioconda   # scope to one conda channel → pixi:bioconda::samtools
ubix search ripgrep --add        # exact match ambiguous across backends → lists `ubix add` cmds to run
```

## Upgrade semantics

`ubix upgrade` unifies install / upgrade / converge / prune:

- **Missing** tool → installed. **Unpinned** → upgraded to latest when behind.
- **Pinned** (`tag` / `version`) → converges to the pin, then skips; `--force`
  reinstalls anyway.
- `--dry-run` reports installed-vs-latest and the chosen action, read-only.
- `--prune` removes **orphans** (in state but not config).

## Bootstrap

`ubix bootstrap` sets up language toolchains/runtimes:

- `rust` → rustup-init (`rustc`/`cargo` in `~/.cargo/bin`)
- `go` → latest stable Go into `$GOROOT` (default `~/.local/share/go`)
- `python` / `nodejs` → a default runtime (uv / fnm) for the pypi/npm sources
- `pixi` → installs `pixi` (from `github:prefix-dev/pixi`) for the pixi source;
  `pixi global` tools land in `$PIXI_HOME/bin` (default `~/.pixi/bin`, add to PATH)

`uv` and `fnm` themselves are ordinary GitHub-release tools — install them with
`ubix add` (each source prints the exact spec when missing).

## Files & environment

- Config: `~/.config/ubix/config.toml`
- State:  `~/.local/share/ubix/state.toml`
- Tokens: `UBIX_GITHUB_TOKEN`, `UBIX_GITLAB_TOKEN` (private / rate-limited repos)

Location overrides, highest precedence first:

| Variable | Effect |
|---|---|
| `UBIX_CONFIG_DIR` | directory that **directly** holds `config.toml` (no `/ubix` appended) |
| `UBIX_DATA_DIR` | directory that **directly** holds `state.toml` (no `/ubix` appended) |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` | `$XDG_CONFIG_HOME/ubix/config.toml`, `$XDG_DATA_HOME/ubix/state.toml` |
| (none) | `~/.config/ubix/`, `~/.local/share/ubix/` |

An empty value counts as unset, and `~`/`$HOME` are expanded. Use `UBIX_*` when
you want to relocate only ubix's own files: `XDG_*` is also read by the child
processes ubix drives (uv, fnm, cargo, go, pixi), `UBIX_*` is not.
`ubix doctor` prints the effective config/state/install paths.

State access is guarded by an exclusive advisory lock; installs stage into a
tempdir and atomically replace the target, so a failed install never corrupts
your `install_dir` or state.

## Docs

- [`docs/PRD.md`](docs/PRD.md) — full product/behavior spec
- [`docs/aqua-plan.md`](docs/aqua-plan.md) — aqua-registry config generator
- [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) — deferred edge cases

## License

MIT
