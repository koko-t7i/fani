# 发布流程和资源契约

fani 的发布版本优先支持 Linux，目前支持采用 glibc 2.31 或更高版本的 Intel/AMD 64 位 Linux（Rust 目标为 `x86_64-unknown-linux-gnu`）。推送 SemVer 标签会运行 [`.github/workflows/release.yml`](../../../.github/workflows/release.yml) 中的 cargo-dist 0.32.0 工作流。该工作流会构建发布版二进制文件，创建 `.tar.xz` 归档文件及其 SHA-256 辅助校验文件，使用 cargo-cyclonedx 0.5.9 生成 CycloneDX XML SBOM，验证归档文件，通过 GitHub 工件证明对最终发布资产进行证明，然后创建 GitHub 发布版本。

发布工作流使用作业级权限。构件构建作业仅具有存储库只读访问权限。cargo-dist 规划作业和最终主机作业会获得 `contents: write`；只有主机作业会获得 `attestations: write` 和 `id-token: write`，以发布并证明最终资产集。每个 `uses:` 引用都固定到完整的提交 SHA。cargo-dist、cargo-cyclonedx、cargo-audit 和 cargo-deny 的版本均固定在配置或工作流中。

## 安装

推荐的安装程序会检测受支持的平台，下载匹配的归档文件，验证其 SHA-256 校验和，并将 `fani` 安装到 `~/.local/bin` 下。它不需要 Rust：

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh | sh
```

在运行安装程序之前验证它：

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh \
  -o fani-installer.sh
gh attestation verify fani-installer.sh --repo koko-t7i/fani
sh fani-installer.sh
rm fani-installer.sh
```

若要改为从源代码构建：

```bash
git clone https://github.com/koko-t7i/fani.git
cd fani
cargo install --path . --locked
```

## 归档内容

平台归档包含一个顶级目录，并且仅允许以下文件：

```text
fani
README.md
_fani
fani.1
fani.bash
fani.fish
fani.service
fani.timer
fani.toml
```

`fani.db`、`.fani/`、提示词、Agent 请求或响应、报告以及临时工作文件均禁止包含。[`scripts/release/verify-archive.sh`](../../../scripts/release/verify-archive.sh) 会检查允许列表、SHA-256 旁车文件、SBOM 标识、Linux ELF 架构、可执行位、版本输出以及帮助入口点。发布工作流会针对 cargo-dist 归档运行此验证器，而不是使用单独构建的测试夹具。

## 验证已下载的发行版

从同一 GitHub Release 下载归档文件、其 `.sha256` 辅助文件以及 `.cdx.xml` SBOM，然后运行：

```bash
sha256sum --check fani-x86_64-unknown-linux-gnu.tar.xz.sha256
gh attestation verify fani-x86_64-unknown-linux-gnu.tar.xz --repo koko-t7i/fani
gh attestation verify fani.cdx.xml --repo koko-t7i/fani
```

校验和用于验证下载的归档文件字节，而 GitHub 证明则用于确认已发布归档文件和 SBOM 的工作流来源。维护者或审计人员如果拥有与确切发布标签对应的源代码检出副本，还可以运行仓库的验证程序：

```bash
scripts/release/verify-archive.sh \
  /path/to/fani-x86_64-unknown-linux-gnu.tar.xz \
  /path/to/fani.cdx.xml
```

验证脚本是源代码树工具，不包含在发布归档中。

## 可复现性范围

对于锁定的发布契约，Shell 安装程序、Linux `x86_64-unknown-linux-gnu` 发布归档、其 SHA-256 辅助文件、统一校验和文件以及 CycloneDX SBOM 均可复现。该契约使用已提交的源代码和 `Cargo.lock`、用于应用程序的 Rust 1.85.0、cargo-dist 0.32.0、cargo-cyclonedx 0.5.9，以及一台 `x86_64` Linux GNU 构建主机。cargo-dist 本身使用 Rust 1.98.0 编译，因为其锁定的工具依赖项所需的编译器版本高于 fani 的 MSRV。

否则，cargo-dist 0.32.0 会将构建时的所有权信息和时间戳复制到 tar 标头中。[`scripts/release/install-reproducible-dist.sh`](../../../scripts/release/install-reproducible-dist.sh) 会验证已发布的 cargo-dist 0.32.0 和 axoasset 2.0.1 crate 的校验和，在编译 cargo-dist 之前应用最小的 `tar::HeaderMode::Deterministic` 打包修复，并通过常规的 Cargo 软件包状态进行安装。这会改变归档文件的创建过程本身；发布资产不会在打包后被重写或规范化。

除非 Cargo 检测到稳定的工作区路径，否则 cargo-cyclonedx 会包含检出目录的绝对路径。[`scripts/release/build-reproducible-release.sh`](../../../scripts/release/build-reproducible-release.sh) 仅在生成 SBOM 时，将每个干净的检出目录绑定挂载到 `/workspace`，并将 `SOURCE_DATE_EPOCH` 设置为源代码提交时间。它会继承 `HOME`、`CARGO_HOME`、`RUSTUP_HOME`、XDG 目录、缓存、配置、凭据以及已安装软件包的状态。构建包装脚本还会在调用 cargo-dist 之前将 `umask` 固定为 `022`。

[`scripts/release/verify-reproducible-release.sh`](../../../scripts/release/verify-reproducible-release.sh) 会创建两个相互独立的干净克隆，并为其使用不同的目标目录；在每个克隆中运行 cargo-dist 的本地/全局发布流程，并要求安装程序、归档文件、校验和文件及 SBOM 在字节层面完全一致。它还会比较提取出的二进制文件的 SHA-256 和 ELF 构建 ID、所有 completion/man/systemd/example/README 文件的字节内容，以及提取出的类型、模式、所有者、所属组、大小、时间戳和链接元数据。该测试工具会打印被测源代码精确修订版本的哈希值；对于下载的构件，已发布的版本校验和仍是权威值。

此声明不涵盖其他目标平台、架构、libc 系列、主机发行版、内核/工具版本、源代码修订版或依赖项锁定文件。每个候选发布版本都必须再次通过双克隆测试框架。

## 安装归档资源

验证后，解压归档文件并进入其唯一的顶级目录：

```bash
tar -xJf fani-x86_64-unknown-linux-gnu.tar.xz
cd fani-x86_64-unknown-linux-gnu
install -Dm755 fani "$HOME/.local/bin/fani"
install -Dm644 fani.1 "$HOME/.local/share/man/man1/fani.1"
install -Dm644 fani.bash "$HOME/.local/share/bash-completion/completions/fani"
install -Dm644 _fani "$HOME/.local/share/zsh/site-functions/_fani"
install -Dm644 fani.fish "$HOME/.config/fish/completions/fani.fish"
install -Dm644 fani.toml "$HOME/.config/fani/fani.toml"
install -Dm644 fani.service "$HOME/.config/systemd/user/fani.service"
install -Dm644 fani.timer "$HOME/.config/systemd/user/fani.timer"
install -d -m 0700 "$HOME/.local/state/fani"
systemctl --user daemon-reload
```

打包的服务使用 `$HOME/.local/bin/fani`、`$HOME/.config/fani/fani.toml` 和 `$HOME/.local/state/fani/reports`，与上述命令一致。启用定时器之前，请替换带注释的示例值，以 `0600` 模式创建 `$HOME/.config/fani/env`，并将每个已配置仓库的父目录添加到服务的 `ReadWritePaths` 中。请先手动运行完全相同的 `ExecStart` 命令。

## 维护者检查

打标签前，请运行已锁定的质量和供应链门禁：

```bash
rustup toolchain install 1.85.0 --profile minimal
rustup toolchain install 1.98.0 --profile minimal
cargo +1.98.0 install --locked cargo-audit@0.22.2 cargo-deny@0.20.2 cargo-cyclonedx@0.5.9
scripts/release/install-reproducible-dist.sh

cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
cargo +1.85.0 check --locked --all-targets
cargo +1.98.0 audit
cargo +1.98.0 deny check
dist generate --check
sudo install -d -m 0755 /workspace
scripts/release/verify-reproducible-release.sh
```

签入的工作流在 cargo-dist 生成的默认配置基础上，特意实施了最小权限、确定性打包和冒烟测试强化，因此应仔细审查 cargo-dist 的更新，而不是直接覆盖工作流却不重新应用这些约束。`/workspace` 必须是未挂载的目录，并且验证程序需要拥有非交互式 `sudo` 权限，才能执行临时绑定挂载。无论成功、中断还是失败，测试工具都会移除该挂载。
