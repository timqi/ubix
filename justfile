# ubix — build & install
bin_dir := env_var_or_default("XDG_BIN_HOME", env_var("HOME") / ".local/bin")

# 编译 release 并安装到 ~/.local/bin
install:
    cargo build --release
    mkdir -p "{{bin_dir}}"
    install -m 0755 target/release/ubix "{{bin_dir}}/ubix"
    @echo "installed -> {{bin_dir}}/ubix"
