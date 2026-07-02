# ubix × aqua-registry 集成设计（方案 B：配置生成器）

> 目标：复用 aqua-registry（~5000 包）人工精选的资产选择模板，摆脱 ubi 启发式在多资产
> 仓库上的误挑。**aqua 只作为「配置生成源」**：把 aqua 包解析、渲染成一段**标准
> `github:` 配置**（spec + 平台 `matching` 表 + `exe`/`rename`）写入 `config.toml`。
> **不新增 `SourceKind::Aqua`**，不进 install 分发路径；aqua 前缀不落盘。
>
> - 状态：**Approved**（codex-expert 评审 PASS，2026-07-02；两条阻塞项已并入 §5/§7）
> - 关联：PRD §4.2 spec、§4.4 字段、§4.7 跨平台 matching、§5.1 github 源
> - 决策：用户已选**方案 B**（生成器），放弃持久 `aqua:` 运行时源。

---

## 0. 为什么是生成器而非持久源（已定）

| | 持久 `aqua:` 源（否决） | **配置生成器（采用）** |
|---|---|---|
| dotfile 可移植性 | 每次 sync 都依赖 aqua 上游可用性/schema 稳定/本地模板引擎正确性 | 自包含、可审计、可手改；行为冻结，用户主动 re-run 才更新 |
| 侵入 | 破坏 `SourceKind` 穷尽匹配契约（install_tool/outdated/all/FromStr），且无统一 latest/install 语义 | 零侵入：`src/aqua/` 纯工具模块，仅被 CLI 的 add/search 调用 |
| 版本嵌名坑 | 每次现算 latest+pin，需运行时 render | 用**平台 matching 表 + 去版本尾段**规避（见 §5） |

生成器天然统一了两个能力：`search` 打印配置片段，`search --add` / `add aqua:x` 写入同一份片段。

---

## 1. 证据基线（23 个热门包实测，2026-07-02）

抽样：cli/cli, openai/codex, sharkdp/{bat,fd}, BurntSushi/ripgrep, eza, fzf, lazygit, dagger,
helm, uv, jq, deno, bun, goreleaser, cosign, golangci-lint, k9s, starship, zoxide, zellij, yq, shellcheck。

**模板 token 使用频率**（命中文件数 / 23）：
- `{{.Arch}}` 23、`{{.OS}}` 22、`{{.Format}}` 19、`{{.Version}}` 14、`trimV` 7、`{{.AssetWithoutExt}}` 6
- `{{.SemVer}}` **0**、`toLower` **0**、模板 pipeline `|` **0**

**结构字段**：
- `version_constraint:"true"` 出现在 **21/22** 文件（`version_overrides` 末尾分支）
- `overrides:` 23/23、`version_overrides:` 22/23（**merge 必做**）
- `version_prefix:` 3（含 codex `rust-`）、`supported_envs:` 18、`no_asset:` 6
- `format:` 分布：tar.gz 99 / zip 77 / **raw 21** / tar.xz 3 / gz 3 / **zst 2**(codex) / regexp 2
- `format_overrides:` **0**、`type: github_release` 22、`type: http` 1、`type: go_install` 0
- `rosetta2:` 16、`windows_arm_emulation:` 16（均为 hint，非阻塞）

**结论**：codex 评审担心的 `.SemVer`/`toLower`/pipeline/`format_overrides` 在热门集里零命中；
`"true"` 分支几乎总存在。故 MVP 模板子集可以很小，degrade 分支覆盖长尾即可。

**ubi 0.9 能力核实**（`~/.cargo/registry/.../ubi-0.9.0/src/extension.rs`）：
`Extension` 枚举含 `Zst`/`TarZst`/`Tzst`（codex `.zst` ✓）、`Exe` 及裸二进制（`format: raw` ✓）、
tar.{gz,xz,bz,bz2}、zip、7z。依赖 `zstd`/`xz2`/`flate2`/`zip` 齐全。**顶层技术风险清除。**

---

## 2. 范围（MVP）

**做**：
- `type: github_release` 单包。
- 版本分支：`version_constraint` 为 `"true"`（首选）；无 `"true"` 时支持单个比较式
  `semver("<op> X")` / `Version <op> "vX"`（op ∈ `<,<=,>,>=,==`），拿现算 latest 求值，取首个命中。
- merge：`base ⊕ 命中 version_override ⊕ 命中 goos/goarch overrides`（后者仅覆盖显式键）。
- 字段：`asset` `format` `files[].{name,src}` `replacements` `supported_envs` `version_prefix` `overrides` `no_asset`。
- 模板 token：`{{.OS}} {{.Arch}} {{.Format}} {{.Version}} {{.AssetWithoutExt}}` + 函数 `trimV`；
  容忍单层 pipeline `{{X | trimV}}`（把 `trimV .Version` 与 `.Version | trimV` 视作等价）。
- 平台：**linux + darwin**，各 amd64/arm64（生成 `PerPlatform` matching 表）。
- 输出：标准 `github:` 配置块（见 §6）。

**不做（命中即 degrade：硬报错 + 打印 registry.yaml 链接，让用户手写 config）**：
- 非 `github_release` 类型（`http`/`go_install`/`cargo`…）。
- `rosetta2`（darwin-arm64 回落 amd64）、`windows*`、windows 平台。
- `.SemVer`、任意 `toLower`/多函数 pipeline、`format_overrides`、`format: regexp`。
- 复杂 `version_constraint` 表达式（`and`/`or`/`semverWithVersion`…）。
- `checksum`（记录忽略；ubi 自行处理 github 资产下载，ubix 记已装二进制 sha256，同 §8.8）。

---

## 3. 模块与依赖

新增 `src/aqua/mod.rs`（纯工具模块，**不**依赖 install 分发；被 CLI add/search 调用）：

```
src/aqua/
  mod.rs        // 对外：resolve_package(), generate_config_block(), search_index()
  registry.rs   // 数据源：per-pkg raw 拉取 + root 索引缓存/刷新
  schema.rs     // serde 反序列化 aqua registry.yaml 子集
  resolve.rs    // 分支选择(version_constraint) + 分层 merge + supported_envs 判定
  template.rs   // Go 模板子集渲染 + replacements + AssetWithoutExt 派生
  synth.rs      // Effective → ubix ToolConfig（github spec + PerPlatform matching + exe/rename）
```

**依赖关系**：`aqua` → `outdated::latest_version`（求 latest 供 semver 求值与去版本渲染）、
`config::{ToolConfig,PlatformString}`、`http::HttpClient`（复用 seam，可 mock）、`platform`。
**反向**：仅 `cli` 依赖 `aqua`。install 分发路径（`install_tool`）完全不感知 aqua。

**新增 crate 依赖**：`serde_yml`（`serde_yaml` 已归档；`serde_yml` 为活跃维护 fork，API 兼容）。
仅 aqua 路径用到，评估是否 `optional`/feature-gate（默认开）。

---

## 4. 数据源（决策1：拆分）

- **`aqua:owner/repo` 直取**：GET `https://raw.githubusercontent.com/aquaproj/aqua-registry/main/pkgs/<owner>/<repo>/registry.yaml`（几 KB，走 `HttpClient`，**免克隆**）。这是 `add`/`search owner/repo` 的主路径。
- **`search <name>` 模糊匹配**：需名字索引 → 拉取 root `registry.yaml`（全包内联，实测约 3–4MB / 10 万行）缓存到 `~/.cache/ubix/aqua-registry.yaml`（尊重 `$XDG_CACHE_HOME`），带拉取时间戳。
  - 首次 search 若缓存缺失 → 自动拉取并提示。
  - `ubix aqua update` 手动刷新（打印新旧时间戳/大小）。
  - 展示缓存 age，提醒可能过期；**安装仍现拉 per-pkg**，故过期只影响 search 展示、不影响正确性。
- 版本发现：**不**信任 registry 里的版本；渲染前用 `outdated::latest_version(github:owner/repo)` 现算 latest（honors `UBIX_GITHUB_TOKEN`）。

> 相比浅克隆：免 git 依赖、免 .git 体积、逻辑更简单。代价：root 文件较大但一次性缓存可接受。

---

## 5. 版本嵌名问题与 matching 生成规则（核心正确性）

ubi `.matching()` 是**大小写敏感子串 `contains`**、固定值，无法感知版本号。资产名分两类：

1. **不含版本**（如 codex `codex-{{.Arch}}-{{.OS}}.{{.Format}}`）→ 渲染出**完整资产名**作 matching：
   linux/amd64 → `codex-x86_64-unknown-linux-musl.zst`。唯一命中。
2. **含版本**（如 gh `gh_{{trimV .Version}}_{{.OS}}_{{.Arch}}.{{.Format}}`）→ **不能**把版本冻进 matching。
   规则：以 `{{...Version...}}` token 为切点，取渲染后的**版本无关片段**作 matching：
   - 优先取**版本 token 之后**的尾段（含 OS/Arch/Format 渲染后的字面）：gh linux/amd64 → `_linux_amd64.tar.gz`。
   - 尾段仍是子串唯一标识（`contains` 命中 `gh_2.65.0_linux_amd64.tar.gz`）。

**边界与失败模式**（codex 评审提出，明确应对）：
- **版本在结尾、之后无字面**（`tool_{{.OS}}_{{trimV .Version}}`）→ 尾段为空。对策：改取
  **版本 token 之前**的最长字面段（`tool_linux_`）；若前后皆不足以唯一区分 → **degrade 报错**
  （提示手写 matching），**绝不**产出空串（空串在 ubix = 无过滤，会静默回退 ubi 启发式）。
- **版本在中间、两侧片段都短**（如仅剩 `-`/`_`）→ 用「前缀字面 + 后缀字面」拼接判定唯一性；
  仍不唯一 → degrade。
- **无版本 token** → 用完整渲染名（最稳）。

**唯一性自检**：生成后可选地对该平台把候选资产名与「同模板换其他 arch/os 渲染出的名字」比对，
确保 matching 子串不会同时命中多个目标（例如 `amd64`/`arm64` 都含 `_linux_`）。MVP 采用「尽量长的
版本无关片段」并在片段仅为分隔符时 degrade；完整交叉校验列为后续增强。
> **MVP 必须实际 degrade（硬报错），不得静默产出过短片段。** 已知局限（记录在案）：MVP 只在
> `linux-amd64` 值 `_linux_amd64.tar.gz` 与 `linux-arm64` 值 `_linux_arm64.tar.gz` **彼此之间**校验
> 唯一性，**不**对同一 release 的全部资产清单校验。故若上游新增 `foo_linux_amd64_v2.tar.gz` 与
> `foo_linux_amd64.tar.gz` 并存，尾段可能误命中——依赖用户 re-run 或后续「全资产交叉校验」增强项兜底。

> 因为是**每平台单独渲染**，matching 表天然是 `PlatformString::PerPlatform`，每键一个平台专属子串，
> 不同 arch 的键互不干扰——`linux-amd64` 键值 `_linux_amd64.tar.gz` 只用于该平台解析（§4.7）。

---

## 6. 生成的配置形态（synth）

`aqua` 解析产出中间结构 `EffectivePkg`，再 synth 成 `ToolConfig`：

```toml
# ubix add aqua:openai/codex  →
[tools.codex]
spec = "github:openai/codex"
exe  = "codex"                 # files[0].name
[tools.codex.matching]         # 每平台渲染的（版本无关）资产名
linux-amd64  = "codex-x86_64-unknown-linux-musl.zst"
linux-arm64  = "codex-aarch64-unknown-linux-musl.zst"
darwin-amd64 = "codex-x86_64-apple-darwin.zst"
darwin-arm64 = "codex-aarch64-apple-darwin.zst"
```
```toml
# ubix add aqua:cli/cli  →
[tools.gh]
spec = "github:cli/cli"
exe  = "gh"
[tools.gh.matching]
linux-amd64  = "_linux_amd64.tar.gz"   # 版本无关尾段
linux-arm64  = "_linux_arm64.tar.gz"
darwin-amd64 = "_macOS_amd64.zip"      # darwin→macOS(replacements), format zip(base)
darwin-arm64 = "_macOS_arm64.zip"
```

**字段映射**：
- `spec` = `github:<repo_owner>/<repo_name>`。
- 工具名（config key）：`--name` 优先；否则 `files[0].name`（命令名，如 `gh`）；再否则 repo_name。
- `exe` / `exes`：单 file → `exe = files[0].name`；多 file → `exes = [names…]`。
- `rename`：当**归档内可执行名 ≠ 期望名**时（`basename(src) != name`，如 codex 的 `.AssetWithoutExt`
  解压名 ≠ `codex`）→ 借助 ubi 单二进制识别 + `rename`。`exe`+`rename` 组合按 §5.1 处理；
  `exes` 与 `rename` 互斥（沿用 github 源既有校验）。nested `src`（如 gh `.../bin/gh`）→ ubi 递归找名，取 `exe=basename`。
- 平台键：`supported_envs` ∩ {linux,darwin}×{amd64,arm64}；`no_asset`/未支持的平台键**不写入**表
  （若当前机器平台恰好不被支持，`add` 时明确报错）。
- `[settings].tag` 不写（保持 latest 语义）；version_prefix 仅影响渲染期版本串（见 §7）。

> **生成后即验证**：synth 完立即用现有 github install 路径实际安装一次（`add` 语义本就即时安装），
> 失败则不落 config（复用 install-first-then-persist，§cli cmd_add）。这把「模板渲染对不对」的
> 验证下沉到真实安装，避免写出装不上的 config。

---

## 7. 解析细节（resolve + template）

**分支选择**（`resolve.rs`）：
1. 顶层包字段作 base。若顶层 `version_constraint:"false"` → base 仅供 merge 继承，不单独成型。
2. 遍历 `version_overrides`（保序）：找 `version_constraint=="true"` 的分支（通常在末尾）→ 命中。
3. 无 `"true"`：对每个 override 求值其比较式（支持 `semver("<op> X")` 与 `Version <op> "vX"`），
   拿 §4 现算的 latest 比较，取**首个**为真者。`semver` 比较用轻量语义化版本比较（split `.`、数值比较、
   忽略 pre-release 精细规则，够 MVP）。
4. 都不命中/表达式不认识 → degrade 报错 + registry.yaml 链接。

**merge**（`base ⊕ branch ⊕ override`）：逐字段浅覆盖，`replacements` 做 map 合并（override 键胜），
`files` 整体替换（若 branch/override 显式给出，否则继承）。选 `overrides` 条目：匹配当前 `goos`
（+可选 `goarch`）的首条。

**模板渲染**（`template.rs`）：
- 变量：`.OS`=goos、`.Arch`=goarch（均先过 `replacements`）、`.Format`=merge 后的 format、
  `.Version`=latest（去掉 `version_prefix` 前缀后的串）、`.AssetWithoutExt`=渲染出的 asset 去掉 `.<format>` 尾。
- 函数：`trimV`（去前导 `v`）。`.Version | trimV` 与 `trimV .Version` 等价处理。
- `version_prefix`：latest tag 若带该前缀（codex `rust-v0.20.0`）→ 供 `.Version` 用的是去前缀（`0.20.0`）。
  codex 的 asset 模板不含 `.Version`，故对 codex 无副作用，但通用需要。
- 两趟：先定 format → 再渲染 asset → 再派生 `.AssetWithoutExt` → 用于 `files[].src`。
- 未知变量/函数/pipeline → 硬报错（含 token 名 + registry.yaml 链接）。
- `format: raw` → 资产无扩展名，`.Format` 为空、`.<format>` 尾不剥；ubi 按裸二进制安装（已核实）。
- **`.AssetWithoutExt` 精确定义**（含 `raw`）：`.AssetWithoutExt` = 渲染后的 asset 去掉尾部
  `.<format>`（当 `format` 非空且 asset 确以该尾结束时才剥）。**当 `format: raw` 时不剥，等于
  asset 本身**。这保证 `raw` + `{{.AssetWithoutExt}}`（见 §1 的 6/23 用法）组合有确定值，
  不产生未定义行为。`version_prefix` 剥离顺序（避免双剥）：先从 `outdated::latest_version()`
  返回的 tag **剥掉 `version_prefix` 前缀**（codex `rust-v0.20.0` → `v0.20.0`），**再**对该结果用
  `trimV`（→ `0.20.0`）；`trimV` 永远作用于已剥前缀的串，不作用于原始 tag。

**merge 里 `overrides` 条目选择（明确优先级，非纯 list 序）**：一个 `overrides` 条目可只写
`goos`、或同时写 `goos`+`goarch`。**匹配优先级 = 具体度优先**：`goos+goarch` > 仅 `goos` >
无约束；**同具体度内**再按 list 出现序取首个。即更具体的条目**无视 list 顺序**胜出，避免「某平台
同时命中 goos-only 与 goos+goarch 两条」时因排序产生错误 format/asset。

**supported_envs 判定**：条目形如 `<goos>` / `<goarch>` / `<goos>/<goarch>` / `all`。
platform (goos,goarch) 支持 ⟺ 存在条目 == `all` 或 == goos 或 == goarch 或 == `goos/goarch`。
`no_asset:true`（出现在某 override 内）视为该平台不可用。

---

## 8. CLI 变更

- 新子命令 `ubix search <query> [--add] [--name N]`：
  - `query` 含 `/` → 当 `owner/repo` 直取 per-pkg；否则在 root 索引里对 **repo 名**模糊匹配
    （**命令名仅在 per-pkg `files[].name`，root 索引无**，故不承诺按命令名搜；命中多个则列出候选让用户复制精确 `owner/repo`）。
  - 默认**只打印**生成的 config 片段（§6）+ 该平台将选中的资产名预览。
  - `--add`：写入 config **并即时安装**（同 `add` 语义，install-first-then-persist）。
- `ubix add aqua:<owner/repo>`：在 `cmd_add` 里识别 `aqua:` 前缀作**预处理**——调用
  `aqua::generate_config_block()` 得到标准 `ToolConfig`（spec 变为 `github:`），其余流程与普通 `add` 完全一致。
  **`aqua:` 不进 `parse_spec`/`SourceKind`**；只在 CLI 层拦截。
- `ubix aqua update`：刷新 root 索引缓存。（`aqua` 作为 CLI 下的一个小命令组：`update`；未来可加 `which`。）
- `sources`：`aqua` 作为「生成器」在帮助文本里单列说明，不列入 `SourceKind::all()`。

**决策3 确认**：search 默认只打印、`--add` 才写。

---

## 9. 降级契约（degrade）

任何 MVP 不支持的构造 → **不写 config、不安装**，报错信息含：
`unsupported aqua construct: <what> for <owner/repo>; see https://github.com/aquaproj/aqua-registry/blob/main/pkgs/<owner>/<repo>/registry.yaml and add a `github:` entry manually`。
覆盖：非 github_release、未知模板 token/函数、无可求值分支、matching 无法唯一化、当前平台无资产。

---

## 10. 测试策略（全部走 mock，无网络）

- `template.rs`：token/函数/pipeline/AssetWithoutExt/version_prefix/未知 token 报错——纯函数单测。
- `resolve.rs`：分支选择（true / 无 true+semver / Version==）、三层 merge、overrides 命中、supported_envs、no_asset。
- `synth.rs`：codex（zst+rename+PerPlatform）、gh（去版本尾段+darwin→macOS+linux override 换 tar.gz）、
  纯无版本单资产、raw 无扩展、多 file→exes。用 §1 抓下的真实 registry.yaml 作 fixture（存入 `tests/fixtures/aqua/`）。
- `registry.rs`：per-pkg 拉取用 `MockHttp`；root 索引解析用截断 fixture。
- CLI：`search` 打印快照、`add aqua:` 预处理产出的 ToolConfig 断言（复用 FakeEngine 免网络安装）。
- 回归：codex/gh 两个 PRD 明确误挑案例，断言生成的 matching 精确命中预期资产名。

---

## 11. 落地顺序（先原型验证高风险，再铺开）

1. **template.rs**（最高风险）：对 §1 的 23 个真实 registry.yaml 跑渲染，确认 token 子集足够、
   degrade 边界清晰。产出「哪些包 MVP 覆盖 / 哪些 degrade」清单。
2. **resolve.rs**：分支选择 + merge，对 gh（有历史 semver 分支）、codex（version_prefix+true）验证。
3. **synth.rs + §5 matching 规则**：codex/gh 端到端生成 config，尤其去版本尾段唯一性。
4. registry.rs 数据源 + 缓存。
5. CLI（search / add aqua: / aqua update）。
6. 全量 mock 测试 + `cargo build`/`cargo test`；codex/gh 真实安装冒烟（可选，需网络）。

---

## 12. 待确认/已知取舍

- `serde_yml` 依赖引入（vs 手写极简 YAML 子集解析——不推荐，YAML 缩进/多态易错）。
- root 索引 3–4MB 缓存可接受；若嫌大，后续可换 sparse checkout 或按首字母分片拉取。
- rosetta2/windows/其他 type 明确 out-of-scope，靠 degrade 兜底，后续按需扩展。
- matching「版本无关片段」规则的交叉唯一性校验 MVP 从简，列为增强项。
