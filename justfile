# ubix — build & install
bin_dir := env_var_or_default("XDG_BIN_HOME", env_var("HOME") / ".local/bin")

# 编译 release 并安装到 ~/.local/bin
install:
    cargo build --release
    mkdir -p "{{bin_dir}}"
    install -m 0755 target/release/ubix "{{bin_dir}}/ubix"
    @echo "installed -> {{bin_dir}}/ubix"

# 打 CalVer 标签 vYYYYMMDD-<shorthash> 并推送 origin，触发 GitHub Release workflow
release:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -n "$(git status --porcelain)" ]; then
        echo "error: working tree not clean — commit before releasing" >&2
        exit 1
    fi
    git fetch --tags --quiet
    TAG="v$(date +%Y%m%d)-$(git rev-parse --short HEAD)"
    if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null 2>&1; then
        echo "error: tag $TAG already exists (nothing new to release)" >&2
        exit 1
    fi
    git tag -a "$TAG" -m "release $TAG"
    git push origin "$TAG"
    echo "pushed $TAG — GitHub Release workflow will build & publish"
