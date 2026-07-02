# ubix — 产品需求文档 (PRD)

> 项目名 `ubix`（= `ubi` + x，扩展版：多来源 + 配置追踪）。一个用 Rust 实现、面向单机用户的**声明式二进制/CLI 工具安装管理器**：
> 把散落在 GitHub/GitLab Release、直链、PyPI、npm、cargo、go 的命令行工具，用一份可版本化的配置，统一安装 / 原地升级 / 干净卸载到 `~/.local/bin`。

- 状态：**Approved**（codex 评审 round 2 通过，PASS）
- 最后更新：2026-07-02

---

## 0. 决策记录（已锁定）

| # | 决策 |
|---|------|
| D1 | **实现语言 = Rust**，直接依赖 `ubi` library crate（v0.9+，`UbiBuilder` API），复用其资产匹配 / 解压启发式。 |
| D2 | **配置与状态两层分离**：`config.toml` 放 `~/.config/ubix/`（人手写、进 dotfiles）；`state.toml` 放 `~/.local/share/ubix/`（机器写、不进 dotfiles）。 |
| D3 | **幂等**：CLI 子命令负责写配置并即时安装；`sync` 让系统状态收敛到配置声明，可反复执行。 |
| D4 | **npm/fnm**：fnm 装并 `default` 到 Node 最新 LTS；全局包装在该 default node 上；把 fnm **运行时探测到的** alias bin 目录加入 PATH（不逐包软链）。 |
| D5 | **配置紧凑语法**：每个工具用 `spec = "$source:$locator"` 一行声明，需要参数时补字段；CLI `ubix add <spec>` 复用同一语法。 |
| D6 | **Token 环境变量**：`UBIX_GITHUB_TOKEN` / `UBIX_GITLAB_TOKEN`。 |
| D7 | **cargo 来源落地 `~/.local/bin`**：透传 `cargo install --root ~/.local`；卸载 `cargo uninstall --root ~/.local`，生命周期由 cargo 账本（`~/.local/.crates.toml`）管理。 |
| D8 | **go 来源落地 `~/.local/bin`**：`GOBIN=~/.local/bin go install`；go 无账本，由 ubix `state.toml` 记账与卸载。 |
| D9 | **工具链引导**：`ubix bootstrap rust|go` 一次性引导；后续版本管理交回 rustup / go 的 `GOTOOLCHAIN`。**Go GOROOT 默认 `~/.local/share/go`**。 |
| D10 | **孤儿处理**（在 config 已删、state 仍在）：`sync` 默认**只报告不删**；`sync --prune` 才卸载孤儿。见 §8.3。 |
| D11 | **锁定 `tag` 后 upgrade**：`upgrade` 对已 pin `tag` 的工具默认**跳过并提示**，`--force` 才重装同 tag。见 §8.4。 |
| D12 | **并发安全**：所有写操作对 `state.toml` 加**排他 advisory 文件锁**（flock），获取不到锁默认 fail-fast（`--wait` 可等待）。见 §8.6。 |
| D13 | **schema 版本**：`config.toml` 与 `state.toml` 均带 `schema_version`（当前 = 1），不匹配时按迁移策略处理。见 §4.6。 |
| D14 | **remove 安全**：只删 `state.toml` 记录的、ubix 自己装的文件；未记录的文件**拒绝删除**并提示。见 §8.5。 |
| D15 | **原子替换 + 校验前移**：原子替换是正确性保证，放入 **M1**（非最后打磨）。见 §11。 |

### 仍开放
| # | 问题 |
|---|------|
| O1 | ~~命名~~ **已定名 `ubix`**（crates.io 空闲，2026-07-02 查重）。 |
| O2 | **`spec` 裸字符串糖**：是否额外支持 `eza = "github:…"`？当前统一用 `spec` 字段。 |

---

## 1. 背景、目标与非目标

### 1.1 问题
- 常用 CLI 工具来源分散：GitHub/GitLab Release 预编译二进制、直链、PyPI、npm，以及 Rust/Go 源码工具。
- 手动安装要自己判断该下哪个 asset（OS/arch/libc/格式）；升级要手动重下删旧；缺少单一清单记录“装了什么、用什么参数装的”，换机器无法一键复现。

### 1.2 目标
1. 一份可版本化的配置，声明所有工具及其安装参数。
2. 多来源统一：release 二进制（复用 ubi 启发式）/ 直链 / PyPI(uv) / npm(fnm) / cargo / go。
3. 尽可能全部落到 `~/.local/bin`，**原地升级**（装最新、删旧），不分版本目录、不做 shims。
4. 缺失的底层工具（uv、fnm、rustup、go）可由 ubix 引导安装。
5. 幂等：`sync` 让系统状态收敛到配置声明。
6. 生命周期完整：安装 / 升级 / **可追踪** / **可干净卸载**。

### 1.3 非目标
- 不替代系统包管理器（apt/brew/pacman）。
- 不做多版本并存、版本切换、shims（PyPI/npm 的隔离由 uv/fnm 天然提供，不在此列）。
- 不管理语言工具链的**版本升级**（交给 rustup / go 的 GOTOOLCHAIN）；ubix 只做一次性引导。
- 首期不支持 Windows（假设类 Unix shell 与 `~/.local/bin`）。

### 1.4 “统一到 ~/.local/bin” 的准确边界（消除歧义）
目标是“**尽可能**”单目录，实际存在**受控例外**，均已在 PATH 与卸载矩阵（§8.7）中登记：
- 落 `~/.local/bin`：github/gitlab/url、pypi(uv, 符号链接形式)、cargo、go。
- 落各自目录并加入 PATH：npm（fnm default node bin）、rust 工具链（`~/.cargo/bin`）、go 工具链（`~/.local/share/go/bin`）。
- ubix 通过 `doctor` 统一检查上述所有 PATH 段是否就绪。

---

## 2. 用户与场景
- 主用户：单人开发者 / 运维，管理自己的 `~/.local/bin`。
- 场景：
  - 新机器初始化：`ubix sync` 恢复全部工具。
  - 加新工具：`ubix add github:owner/repo` / `pypi:ruff` / `npm:pnpm` / `cargo:xx` / `go:...@latest`。
  - 例行升级：`ubix upgrade [name|--all]`。
  - 查状态：`ubix list` / `ubix outdated`。
  - 引导工具链：`ubix bootstrap rust|go`。

---

## 3. 架构概览
- 语言：Rust（edition 2021+）。
- 关键依赖：`ubi`（release 资产匹配+解压）、`serde`+`toml`、`reqwest`（直链 / go.dev/dl / outdated 查询）、`clap`、`sha2`、`tempfile`、`fs2`/`nix`（flock）。
- 模块划分（建议）：
  - `config`：读写 `config.toml`，解析 `spec` 为内部 `ToolSpec`，校验 `schema_version`。
  - `state`：读写 `state.toml`（加 flock），迁移。
  - `sources/*`：每来源一个 handler，实现统一 trait `Source { resolve_latest, install, upgrade, remove, installed_version }`。
  - `engine`：release 安装引擎（封装 `ubi::UbiBuilder`）。
  - `bootstrap`：uv / fnm / rustup / go 引导。
  - `cli` / `paths`（XDG 解析、PATH 检查）。
- **无守护进程**：纯 CLI，一次调用一次收敛。

---

## 4. 配置与状态

### 4.1 位置（XDG）
- 配置：`~/.config/ubix/config.toml`（尊重 `$XDG_CONFIG_HOME`）——**整个目录进 dotfiles**。
- 状态：`~/.local/share/ubix/state.toml`（尊重 `$XDG_DATA_HOME`）——机器写入，**不进 dotfiles**。

### 4.2 `spec` 紧凑语法（D5）
统一格式：`spec = "$source:$locator"`。config 与 CLI `add` 共用。

| 前缀 | locator 形态 | 示例 |
|---|---|---|
| `github:` | `owner/repo` | `github:eza-community/eza` |
| `gitlab:` | `group[/subgroup…]/repo`（自建加 `host`） | `gitlab:group/repo` |
| `url:` | 固定直链归档/二进制 URL（无版本发现） | `url:https://…/x-linux-x86_64.tar.gz` |
| `http:` | URL **模板** + 版本发现（aqua 式），见 §5.7 | `http:https://…/{version}/{os}-{arch}/claude` |
| `pypi:` | 包名 | `pypi:ruff` |
| `npm:` | 包名 | `npm:pnpm` |
| `cargo:` | crate 名 | `cargo:somecli` |
| `go:` | 模块路径`@`版本 | `go:example.com/cmd/tool@latest` |

**解析规则**：
- `default_source`（默认 `github`）仅在 locator **无前缀**时套用；此时 locator 必须是 `owner/repo`（两段）。**禁止无前缀单词 locator**（如裸 `ruff`），否则报错要求显式前缀。
- `matching` 语义 = ubi 的子串包含（`str::contains`，**大小写敏感**，与 ubi v0.9 一致），非 glob / regex；写 `matching` 时须与 GitHub/GitLab 资产名大小写一致。

### 4.3 config.toml 完整示例
```toml
schema_version = 1

[settings]
install_dir     = "~/.local/bin"
go_root         = "~/.local/share/go"   # go 工具链解压目录（D9）
default_source  = "github"

# 1) release 二进制（复用 ubi 启发式）
[tools.eza]
spec = "github:eza-community/eza"

[tools.codex]
spec     = "github:openai/codex"
matching = "codex-x86_64-unknown-linux"  # 子串消歧（大小写敏感）
exe      = "codex"                        # 归档内可执行名，默认 = key

[tools.uv]
spec = "github:astral-sh/uv"
exes = ["uv", "uvx"]                       # 多入口，见 §5.1

[tools.selfhosted]
spec = "gitlab:group/subgroup/repo"
host = "https://gitlab.fish"               # 自建 GitLab（→ api_base_url，§5.1）
tag  = "v1.2.3"                            # 版本锁定，默认 latest

# 2) 直链
[tools.something]
spec = "url:https://example.com/something-linux-x86_64.tar.gz"
exe  = "something"

# 3) PyPI（uv tool）
[tools.ruff]
spec    = "pypi:ruff"
# version = "0.6.*" ; extras = ["all"] ; with = ["ruff-lsp"]

# 4) npm（fnm default LTS node）
[tools.pnpm]
spec = "npm:pnpm"

# 5) Rust 源码（无预编译时）→ cargo 管账，落 ~/.local/bin
[tools.somecli]
spec = "cargo:somecli"
# features = ["x"] ; version = "1.*" ; locked = true

# 6) Go 源码（无预编译时）→ GOBIN=~/.local/bin，ubix 记账
[tools.gotool]
spec = "go:example.com/cmd/gotool@latest"
```

### 4.4 可选字段（按来源）
| 字段 | 适用来源 | 含义 / 映射 |
|---|---|---|
| `matching` | github/gitlab/url | 子串消歧（ubi `.matching()`，`contains`，大小写敏感）。可为**单字符串**或**按平台键的表**（见 §4.7），并支持 `{os}`/`{arch}` 模板 + `arch_replace`/`os_replace` |
| `exe` | github/gitlab/url | 单入口可执行名，默认 = key（ubi `.exe()`） |
| `exes` | github/gitlab/url | 多入口（实现见 §5.1） |
| `tag` | github/gitlab | 版本锁定，默认 `latest`（ubi `.tag()`） |
| `host` | gitlab | 自建 GitLab base；ubix 追加 `/api/v4` 后传 ubi `.api_base_url()` |
| `rename` | release 类 | 安装后重命名（ubi `.rename_exe_to()`） |
| `version` | pypi/npm/cargo/http | 版本约束 / http 的版本 pin |
| `extras`/`with` | pypi | uv tool extras / `--with` 额外包 |
| `features`/`locked` | cargo | cargo 特性 / `--locked` |
| `url_musl` | http | Linux musl 上的替代 URL 模板（§5.7） |
| `version_source` | http | 版本发现来源，如 `github:owner/repo`（§5.7） |
| `arch_replace`/`os_replace` | http | 运行时 arch/os token → URL token 映射（如 `amd64→x64`） |

### 4.5 state.toml 草案（机器写入）
```toml
schema_version = 1

[tools.eza]
source            = "github"
installed_version = "v0.18.21"
resolved_asset    = "eza_x86_64-unknown-linux-musl.tar.gz"
install_paths     = ["/home/qiqi/.local/bin/eza"]
sha256            = "…"
installed_at      = "2026-07-02T08:45:00Z"
updated_at        = "2026-07-02T08:45:00Z"

[tools.gotool]
source            = "go"
installed_version = "v1.4.0"
module            = "example.com/cmd/gotool"
install_paths     = ["/home/qiqi/.local/bin/gotool"]   # go 无账本 → 卸载依据在此
installed_at      = "2026-07-02T08:45:00Z"
updated_at        = "2026-07-02T08:45:00Z"

[_runtime]                                              # 顶层：记录 fnm default node 版本等运行时事实
node_default      = "v22.14.0"
```

### 4.6 schema 版本与迁移（D13）
- 读入时校验顶层 `schema_version`：
  - 相等 → 正常。
  - 文件版本更低 → 运行内置迁移器升级后写回（state），或提示 config 需手动升级（config 不自动改人手文件，仅提示）。
  - 文件版本更高（比程序新）→ **拒绝运行**并提示升级 ubix，避免误解析。

### 4.7 跨平台 `matching`（dotfile 可移植）
config 通过 dotfiles 在多台机器/平台共享，故 `matching` 必须能因平台而异。`matching` 可写成两种形式：
- **单字符串**（向后兼容）：所有平台同一值。
- **按平台键的表**：键用 `<goos>-<goarch>`（如 `linux-amd64`/`darwin-arm64`）、`<goos>`（如 `darwin`）、或 `*`/`default` 兜底。
- **解析优先级**：`<os>-<arch>` > `<os>` > `*`/`default`。值为空串 `""` 表示「不加 matching，交给 ubi 启发式」→ 等价 None。命中不到且无 `*` → **报错**并提示补该平台键（显式表里缺平台通常是遗漏）。
- 解析出的字符串还支持 `{os}`/`{arch}` 模板 + `arch_replace`/`os_replace`（同 §5.7），便于命名规整的工具用单模板。
- 示例（codex，各平台**格式都不同**，单模板做不到，故用平台表）：
```toml
[tools.codex]
spec = "github:openai/codex"
[tools.codex.matching]
linux-amd64  = "codex-x86_64-unknown-linux-musl.tar.gz"
linux-arm64  = "codex-aarch64-unknown-linux-musl.tar.gz"
darwin-amd64 = "codex-x86_64-apple-darwin.tar.gz"
darwin-arm64 = "codex-aarch64-apple-darwin.zst"
```

---

## 5. 各来源处理逻辑

### 5.1 github / gitlab release（复用 ubi）
复用 ubi 启发式（通过 `ubi::UbiBuilder`）：
1. **平台探测**：OS（linux/macos/*bsd）、arch（x86_64/amd64、aarch64/arm64、arm、i686、ppc64le、s390x、riscv64…）、libc（Linux 区分 musl/gnu，静态优先取 musl）。
2. **资产过滤**：OS → arch → libc → 排除无关扩展（`.deb/.rpm/.msi/.sha256/.asc/.sig` 等）。
3. **消歧**：仍多候选时用 `matching` 子串（`.matching()`）。
4. **格式处理**：解压 `.tar.{gz,xz,bz2,zst}`/`.zip`/`.gz`，或裸二进制、AppImage。
5. **落地**：写入 `install_dir`，`chmod +x`，写 `state.toml`。
- **单入口**：`exe`/默认 → `UbiBuilder::exe()`，`rename` → `rename_exe_to()`。
- **多入口 `exes`**（如 uv/uvx）：实现策略 = **`extract_all()` 解出全部到临时目录，再按 `exes` 白名单挑选、原子移动进 `install_dir`，其余丢弃**（避免多次网络下载）。若归档结构不支持，则回退为对每个 exe 各调一次 ubi。`state.toml` 的 `install_paths` 记录全部入口。
- **自建 GitLab**：`host="https://gitlab.fish"` → ubix 拼 `https://gitlab.fish/api/v4` 传 `UbiBuilder::api_base_url()` 并 `forge(GitLab)`。
- Token：`UBIX_GITHUB_TOKEN` / `UBIX_GITLAB_TOKEN` → ubi `.github_token()/.gitlab_token()`（匿名 GitHub API 仅 60 req/h）。
- **版本记账**：`tag` 已 pin → 记 pin 值。未 pin 时，由于 ubi 0.9 不暴露它解析到的实际 tag，unpinned github/gitlab 会在安装时**额外发一次 releases-API 查询**取真实最新 tag 记入 `installed_version`（复用 `outdated::latest_version`，honors `UBIX_GITHUB_TOKEN`/`UBIX_GITLAB_TOKEN`）；仅当该查询失败时才回退记 `latest`。这样 `list`/`outdated` 能显示真实版本、且 `installed==latest` 时正确判为未过期。

### 5.2 url 直链
- 无 release API 时的兜底：下载给定 URL，按扩展名走 §5.1 格式处理与提取（`extract_all` + `exe`/`exes` 挑选）。
- **无 latest 概念**：视为固定版本，`outdated` 跳过、`upgrade` 需手动改 URL。state 记录 URL 与下载内容 sha256 以判定是否变化。

### 5.3 pypi（uv tool）
- 前置 `uv`：**不做特殊 bootstrap**——uv 本身就是普通 GitHub 单文件 release。缺失时报错并**列出可复制的安装 spec**：`ubix add github:astral-sh/uv --name uv --exes uv,uvx`。
- 安装：`uv tool install <package>[==version][--with …]`。可执行入口以**符号链接**形式出现在 `~/.local/bin`，实体 venv 在 `~/.local/share/uv/tools/<name>/`。
- 升级：`uv tool upgrade <package>`。
- **卸载：一律 `uv tool uninstall <package>`**（删符号链接 + 清理 venv）。**不可直接 rm 符号链接**（会泄漏 venv）。§8.7 矩阵以此为准。

### 5.4 npm（fnm default LTS node，D4）
- 前置 `fnm`：同 uv，**不做特殊 bootstrap**。缺失时报错并列出安装 spec：`ubix add github:Schniz/fnm --name fnm`（asset 命名不规范：x86_64 是 `fnm-linux.zip`，arm 是 `fnm-arm64.zip`；aarch64 上可能需 `--matching`）。装好后 `fnm default <lts>` 使 npm 可用。
- `fnm install --lts` 并 `fnm default <该版本>`。
- **PATH 目录运行时探测**：**不硬编码** `~/.local/share/fnm`。通过 `fnm env --json`（或读 `FNM_DIR`，并处理 legacy `~/.fnm` 回退）取得实际 base，得到稳定 alias bin 目录 `<base>/aliases/default/bin` 加入 PATH。alias 是软链，LTS 大版本升级自动跟随。
- 包操作：`npm i -g <pkg>[@version]` / `@latest` / `npm rm -g <pkg>`。
- **LTS 跃迁检测**：state 记录 `_runtime.node_default`；每次 `sync`/`upgrade` 读 `fnm current`/`fnm default` 实际版本，与记录值比较；不同则在新 default 上**按 config 重装所有 `npm` 来源工具**并更新记录，保持幂等。
- 副作用（已接受）：node/npm/npx 及所有全局 npm 包入口都会进 PATH，不落 `~/.local/bin`。

### 5.5 cargo（源码，D7）
- 前置：rustc/cargo（见 §6.1）。
- 安装/升级：`cargo install --root ~/.local <crate>[--version V][--features …][--locked]` → 二进制落 `~/.local/bin`，账本 `~/.local/.crates.toml`。
- 卸载：`cargo uninstall --root ~/.local <crate>`。
- 生命周期由 cargo 账本管理；ubix 仅在 `state.toml` 记声明。权衡：编译耗时。

### 5.6 go（源码，D8）
- 前置：go 工具链（见 §6.2）。
- 安装/升级：`GOBIN=~/.local/bin go install <module>@<version>` → 落 `~/.local/bin`。
- go **无账本、无 `go uninstall`**：ubix 用 `state.toml` 的 `install_paths` 记账，卸载即删对应文件；`go version -m <bin>` 可反查校验。

### 5.7 http（aqua 式模板 URL + 版本发现）
面向「不在 GitHub Release、而是托管在固定 CDN/GCS，按版本模板下载」的工具（典型：claude-code）。区别于 `url:`（固定链接、无 latest）。
- **spec** = `http:<URL模板>`；模板变量 `{version}` / `{os}`（GOOS：linux/darwin/…）/ `{arch}`（GOARCH：amd64/arm64/…）。替换前套用 `arch_replace`/`os_replace`（映射运行时 token → URL token，如 `amd64→x64`）。
- **版本发现**：优先级 `tag`/`version`（pin） > `version_source`。`version_source = "github:owner/repo"` → 查该仓库最新 release/tag（复用 §7.1 github 查询），去掉前导 `v` 作 `{version}`。两者皆无 → 报错。
- **libc 变体**：Linux musl 上若设了 `url_musl` 用之，否则用 `url`（复用平台 libc 探测）。
- **格式**：按渲染后 URL 扩展名走 §5.2 的解压/提取；无扩展（如 `/claude`）→ 裸二进制。支持 `exe`/`exes`/`rename`。
- **state**：`source="http"`，记 resolved 版本、渲染后的 asset 名、sha256、locator(模板)。
- **outdated**：设了 `version_source` → 查其最新对比已装；否则 `n/a`。
- **升级**：重解析版本后重装（pin `tag` 时按 §8.4 跳过）。
- 配置示例（claude-code）：
```toml
[tools.claude]
spec           = "http:https://storage.googleapis.com/claude-code-dist-<id>/claude-code-releases/{version}/{os}-{arch}/claude"
url_musl       = "https://storage.googleapis.com/claude-code-dist-<id>/claude-code-releases/{version}/{os}-{arch}-musl/claude"
version_source = "github:anthropics/claude-code"
exe            = "claude"
arch_replace   = { amd64 = "x64" }
```
> 注意：claude-code 自带后台自更新，交给 ubix 管会与其自更新相互覆盖/漂移；此源提供能力，是否使用由用户权衡。

---

## 6. 工具链引导（D9）
`bootstrap <rust|go|python|nodejs>`。`rust`/`go` 引导多文件语言工具链（无法当单二进制 `add`）。`python`/`nodejs` 是**便捷编排**：先用普通 `add`（github 源）装好 uv / fnm（幂等、纳入 config 追踪），再让 **uv 装最新稳定版 Python 设默认**（`uv python install --default`）/ **fnm 装最新 LTS 设默认**（`fnm install --lts` + `fnm default <ver>`，修好「无 default node」）。uv / fnm 本身仍是普通 `add` 安装的单文件工具。一次性引导，后续版本升级交回官方工具；工具链不作为普通单二进制 tool 追踪。**幂等**：若目标已存在则默认跳过并提示，`--reinstall` 才重跑。

### 6.1 Rust → rustup
- `rustup-init` 单文件：`url:` 来源从 `https://static.rust-lang.org/rustup/dist/<target>/rustup-init` 拉取，运行 `rustup-init -y`。
- 结果：`rustc`/`cargo` 在 `~/.cargo/bin`（加入 PATH）。版本交回 `rustup update` / `rustup toolchain install`。

### 6.2 Go → 官方 tarball
- 从 `https://go.dev/dl/?mode=json` 取最新 stable 的 `go<ver>.linux-<arch>.tar.gz`。
- 解压到 GOROOT = **`~/.local/share/go`**，`~/.local/share/go/bin` 加入 PATH。
- 版本交回 Go 自带 `GOTOOLCHAIN=auto`（Go 1.21+ 默认）：`go` 命令按模块 `go` 指令自动下载/切换更新工具链。ubix 只保证一个可用 bootstrap go。

---

## 7. CLI 接口
```
ubix add <spec> [--matching S] [--exe E] [--exes A,B] [--tag T] [--host U] [--version V] [--force]
      # spec 语法同 §4.2；写入 config 并立即安装
      # 同名工具已存在时默认报错（提示用 upgrade 或 --force）；--force 才覆盖参数并重装（§8.10）
ubix remove <name>              # 卸载（按来源选路径）+ 从 config 删除；仅删 state 记录文件（D14）
ubix upgrade [name | --all] [--force]   # 原地升级；pin tag 默认跳过（D11）
ubix sync [name] [--dry-run] [--prune] [--wait]  # 幂等对账；给 name 只对账该工具，默认全部
ubix list                       # 已声明工具：名称 / spec / 已装版本
ubix outdated                   # 各来源最新版 vs 已装（查询见 §7.1）
ubix info <name>                # 来源、asset/module、路径、参数
ubix edit                       # 打开 config.toml
ubix doctor                     # 检查 uv/fnm/rustup/go、各 PATH 段、~/.local/bin 就绪
ubix bootstrap <rust|go|python|nodejs> [--reinstall]  # rust/go 工具链；python/nodejs=装uv/fnm并设默认runtime（§6）
```

### 7.1 `outdated` 各来源查询
| 来源 | 最新版查询 |
|---|---|
| github | Releases API（复用 ubi 的 latest 解析） |
| gitlab | GitLab Releases API（`<host>/api/v4/projects/:id/releases`） |
| pypi | `https://pypi.org/pypi/<pkg>/json` 的 `info.version` |
| npm | `https://registry.npmjs.org/<pkg>/latest` 的 `version` |
| cargo | `https://crates.io/api/v1/crates/<name>` 的 `crate.max_stable_version` |
| go | `https://proxy.golang.org/<module>/@latest` 的 `Version` |
| url | 无 latest 概念 → 标记 `n/a` |
| http | 若设 `version_source`（github）→ 查其最新；否则 `n/a` |

---

## 8. 核心行为约定

### 8.1 原地升级
新版本装到同一路径覆盖、删旧文件；不留历史版本、无版本目录、无 shims。

### 8.2 幂等
`sync` 可反复执行；已是目标版本则跳过；npm LTS 跃迁触发重装（§5.4）。

### 8.3 孤儿处理（D10）
state 有、config 无的工具：`sync` 默认**仅列出警告**；`sync --prune` 才按来源卸载并从 state 移除。

### 8.4 pin tag 升级（D11）
工具已 pin `tag`：`upgrade` 默认跳过并提示“已锁定 <tag>”；`--force` 才重装同 tag（用于修复损坏文件）。

### 8.5 remove 安全（D14）
`remove` 只处理 `state.toml` 记录、由 ubix 安装的 `install_paths`。若目标不在 state（如用户预先手放的同名文件）→ **拒绝删除**并提示。`--force` 的语义是**先 adopt（把该文件登记进 state）再删除**，即显式承认“接管并移除”；不提供“删任意未记录文件”的能力。

### 8.6 并发与锁（D12）
所有会写 `state.toml` 的操作先取其**排他 advisory 锁（flock）**；取不到默认 fail-fast 报“另一个 ubix 正在运行”，`--wait` 则阻塞等待。读操作（list/info）不加写锁。

### 8.7 原子替换与失败恢复（D15）
- release/url/go：先下载/编译到临时文件 → 校验 → 原子 `rename` 替换。校验/替换成功后**才**更新 `state.toml`。
- **恢复契约**：若在更新 state 前任一步失败（网络中断、chmod 失败、磁盘满），state 不被写入，下次 `sync` 自动重试；已存在的可用旧二进制不被破坏。

### 8.8 校验和发现
按优先级扫描同 release 资产：`<asset>.sha256` → `<asset>.sha256sum` → 合并文件 `checksums.txt` / `SHA256SUMS`（解析其中匹配 `<asset>` 的行）。找到则验证并写 `state.sha256`；找不到记 `checksum = "none"`（不阻断安装，`doctor` 可提示）。
- **适用范围**：sidecar 发现算法用于 **url 源**（ubix 自行下载、可访问同目录/同 release 资产清单）。对 **github/gitlab release**，资产下载由 ubi 内部完成（不暴露资产清单），故不做 sidecar 扫描；此时 `state.sha256` 记录**已安装二进制的 sha256**（用于跨次安装的篡改/变更检测）。§8.8 的解析/校验逻辑已实现并驱动 url 路径。
- **bootstrap go**：go.dev/dl JSON 每个文件带 `sha256`，解压前用本算法校验 tarball。

### 8.9 PATH 自检
`doctor` 检查以下是否在 `$PATH`，缺失给出写入 shell rc 的建议：`~/.local/bin`、`~/.cargo/bin`、fnm alias bin（运行时探测）、`~/.local/share/go/bin`。

### 8.10 add 已存在保护
`add` 计算出的工具 key 若已在 `config.toml` 中存在，默认**报错并中止**（在安装前，不触网），提示改用 `upgrade <name>`（重装）或 `add --force`（有意覆盖）。`--force` 时整条替换该 config 条目并重装。此举避免静默覆盖已设参数（如 `exe`/`matching`）。注意 key 由 locator 末段派生，`--name` 可显式指定；不同 key 会新建并存条目而非覆盖。

---

## 9. 平台支持
- 首期：Linux x86_64 / aarch64。后续：macOS（arm64/x86_64）。非目标：Windows。

---

## 10. 边界、风险与缓解
- API 限流 → token 环境变量（D6）。
- release 无预编译 / 命名不规范 → `matching`/`exe` 兜底；fnm 即例子。
- npm/fnm 模型张力与 LTS 跃迁 → §5.4。
- cargo 编译慢 → 优先预编译来源，cargo 兜底。
- go 无卸载 → ubix 自记账（§5.6）。
- 多入口归档（uv/uvx） → `exes` + `extract_all` 挑选（§5.1）。
- uv 符号链接泄漏 venv → 卸载强制走 `uv tool uninstall`（§5.3）。
- 并发写 state 竞态 → flock（§8.6）。
- schema 演进 → `schema_version` + 迁移（§4.6）。

---

## 11. 里程碑
- **M1**：config/state 模型（含 `schema_version`、flock）+ `spec` 解析 + github release 安装/升级/删除 + **原子替换（D15）** + `list` + **最小 `sync`（装缺失、不含高级升级逻辑，支撑换机器初始化）**。
- **M2**：完整 `sync`（对账/`--prune`/`--dry-run`）+ gitlab（含自建 `host`→api_base_url）+ token + `doctor` + PATH 管理。
- **M3**：pypi(uv) + uv 引导 + `uv tool uninstall` 卸载语义。
- **M4**：npm(fnm) + fnm/LTS 引导 + PATH 运行时探测 + LTS 跃迁重装。
- **M5**：cargo + go 来源 + rust/go 工具链引导（含 `--reinstall` 幂等）。
- **M6**：`outdated`（§7.1 多来源查询）+ 校验和发现（§8.8）+ url 来源打磨。

---

## 12. 未决实现细节
- `exes` 归档挑选 vs N 次 ubi 调用的最终选型（见 §5.1，倾向 `extract_all` 挑选）。架构已在 §5.1 定型，**M3 前**确认即可（M1 只处理单入口 github 工具，不触发 `exes` 路径）。
- macOS 引入时机与 universal2 处理。

## 13. 开放问题
- **O1 命名**：✅ 已定名 `ubix`（`ubi`+x，crates.io 查重空闲）。
- **O2 `spec` 裸字符串糖**：是否额外支持 `eza = "github:…"`？当前统一 `spec` 字段。
