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
| `url` | built-in download (fixed link) | `url:https://…/x-linux.tar.gz` |
| `template` | built-in download + version discovery (templated URL) | `template:https://…/{version}/{os}-{arch}/bin` |
| `pypi` | `uv tool install` | `pypi:ruff` |
| `npm` | `npm -g` on fnm's default LTS node | `npm:pnpm` |
| `cargo` | `cargo install --root ~/.local` | `cargo:ripgrep` |
| `go` | `GOBIN=~/.local/bin go install` | `go:example.com/cmd/tool@latest` |

A bare `owner/repo` uses `settings.default_source` (default `github`).
Run `ubix sources` for the live list.

## Commands

```
ubix add <spec> [--name N] [--matching S] [--exe E] [--exes A,B] [--tag T]
                [--host U] [--version V] [--rename R] [--force]
ubix upgrade [names…] [--all] [--force] [--dry-run] [--prune]
ubix remove <name> [--force]
ubix list
ubix info <name>
ubix edit                       # open config.toml in $EDITOR
ubix doctor                     # check tools + PATH readiness
ubix bootstrap <rust|go|python|nodejs> [--reinstall]
ubix sources
ubix search <owner/repo | query> [--add] [--name N]   # aqua-registry → github: config
```

Global: `-q/--quiet`, `-v/--verbose`.

### Examples

```sh
ubix add github:eza-community/eza
ubix add pypi:ruff --version 0.6.9
ubix add github:astral-sh/uv --name uv --exes uv,uvx
ubix upgrade --all --dry-run     # preview installed vs latest
ubix upgrade --all               # install missing + upgrade
ubix upgrade --prune ruff        # also drop orphans in scope
ubix search ripgrep --add        # find via aqua-registry and install
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

`uv` and `fnm` themselves are ordinary GitHub-release tools — install them with
`ubix add` (each source prints the exact spec when missing).

## Files & environment

- Config: `~/.config/ubix/config.toml` (honors `$XDG_CONFIG_HOME`)
- State:  `~/.local/share/ubix/state.toml` (honors `$XDG_DATA_HOME`)
- Tokens: `UBIX_GITHUB_TOKEN`, `UBIX_GITLAB_TOKEN` (private / rate-limited repos)

State access is guarded by an exclusive advisory lock; installs stage into a
tempdir and atomically replace the target, so a failed install never corrupts
your `install_dir` or state.

## Docs

- [`docs/PRD.md`](docs/PRD.md) — full product/behavior spec
- [`docs/aqua-plan.md`](docs/aqua-plan.md) — aqua-registry config generator
- [`docs/KNOWN_LIMITATIONS.md`](docs/KNOWN_LIMITATIONS.md) — deferred edge cases

## License

MIT
