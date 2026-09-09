# Release process and asset contract

fani releases are Linux-first and currently target Intel/AMD 64-bit GNU/Linux (`x86_64-unknown-linux-gnu`). The release pipeline does not yet verify a minimum supported glibc version. Pushing a SemVer tag runs the cargo-dist 0.32.0 workflow in [`.github/workflows/release.yml`](../.github/workflows/release.yml). The workflow builds the release binary, creates a `.tar.xz` archive and SHA-256 sidecar, generates a CycloneDX XML SBOM with cargo-cyclonedx 0.5.9, validates the archive, attests the final release assets through GitHub artifact attestations, and then creates the GitHub release.

The release workflow uses job-level permissions. Artifact build jobs have read-only repository access. The cargo-dist planning job and final host job receive `contents: write`; only the host job receives `attestations: write` and `id-token: write` to publish and attest the final asset set. Every `uses:` reference is pinned to a full commit SHA. cargo-dist, cargo-cyclonedx, cargo-audit, and cargo-deny versions are pinned in configuration or workflows.

## Install

The recommended installer detects the supported platform, downloads the matching archive, verifies its SHA-256 checksum, and installs `fani` under `~/.local/bin`. It does not require Rust:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh | sh
```

To verify the installer before running it:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koko-t7i/fani/releases/latest/download/fani-installer.sh \
  -o fani-installer.sh
gh attestation verify fani-installer.sh --repo koko-t7i/fani
sh fani-installer.sh
rm fani-installer.sh
```

To build from source instead:

```bash
git clone https://github.com/koko-t7i/fani.git
cd fani
cargo install --path . --locked
```

## Archive contents

The platform archive has one top-level directory and this exact file allowlist:

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

`fani.db`, `.fani/`, prompts, Agent requests or responses, reports, and temporary work files are forbidden. [`scripts/release/verify-archive.sh`](../scripts/release/verify-archive.sh) checks the allowlist, SHA-256 sidecar, SBOM identity, Linux ELF architecture, executable bit, version output, and help entry point. The release workflow runs this verifier against the cargo-dist archive rather than a separately built fixture.

## Verify a downloaded release

Download the archive, its `.sha256` sidecar, and the `.cdx.xml` SBOM from the same GitHub release, then run:

```bash
sha256sum --check fani-x86_64-unknown-linux-gnu.tar.xz.sha256
gh attestation verify fani-x86_64-unknown-linux-gnu.tar.xz --repo koko-t7i/fani
gh attestation verify fani.cdx.xml --repo koko-t7i/fani
```

The checksum verifies the downloaded archive bytes, and the GitHub attestations establish workflow provenance for the published archive and SBOM. Maintainers or auditors with a source checkout at the exact release tag can additionally run the repository's verifier:

```bash
scripts/release/verify-archive.sh \
  /path/to/fani-x86_64-unknown-linux-gnu.tar.xz \
  /path/to/fani.cdx.xml
```

The verifier script is a source-tree tool and is not included in the release archive.

## Reproducibility scope

The shell installer, Linux `x86_64-unknown-linux-gnu` release archive, its SHA-256 sidecar, the unified checksum file, and the CycloneDX SBOM are reproducible for the locked release contract. The contract uses the committed source and `Cargo.lock`, Rust 1.85.0 for the application, cargo-dist 0.32.0, cargo-cyclonedx 0.5.9, and an `x86_64` Linux GNU build host. cargo-dist itself is compiled with Rust 1.98.0 because its locked tool dependencies require a newer compiler than fani's MSRV.

cargo-dist 0.32.0 otherwise copies build-time ownership and timestamps into tar headers. [`scripts/release/install-reproducible-dist.sh`](../scripts/release/install-reproducible-dist.sh) verifies the published cargo-dist 0.32.0 and axoasset 2.0.1 crate checksums, applies the minimal `tar::HeaderMode::Deterministic` packaging fix before compiling cargo-dist, and installs it through normal Cargo package state. This changes archive creation itself; release assets are not rewritten or normalized after packaging.

cargo-cyclonedx includes the checkout's absolute path unless Cargo sees a stable workspace path. [`scripts/release/build-reproducible-release.sh`](../scripts/release/build-reproducible-release.sh) bind-mounts each clean checkout at `/workspace` only while generating the SBOM and sets `SOURCE_DATE_EPOCH` to the source commit time. It inherits `HOME`, `CARGO_HOME`, `RUSTUP_HOME`, XDG directories, caches, configuration, credentials, and installed package state. The build wrapper also fixes `umask` to `022` before invoking cargo-dist.

[`scripts/release/verify-reproducible-release.sh`](../scripts/release/verify-reproducible-release.sh) creates two separate clean clones with separate target directories, runs the cargo-dist local/global release path in each, and requires byte-identical installers, archives, checksum files, and SBOMs. It also compares the extracted binary SHA-256 and ELF build ID, all completion/man/systemd/example/README bytes, and extracted type, mode, owner, group, size, timestamp, and link metadata. The harness prints the hashes for the exact source revision under test; published release checksums remain the authoritative values for downloaded assets.

This claim does not cover other targets, architectures, libc families, host distributions, kernel/tool versions, source revisions, or dependency lockfiles. Every release candidate must pass the two-clone harness again.

## Install archive assets

After verification, extract the archive and enter its single top-level directory:

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

The packaged service uses `$HOME/.local/bin/fani`, `$HOME/.config/fani/fani.toml`, and `$HOME/.local/state/fani/reports`, matching the commands above. Before enabling the timer, replace the annotated example values, create `$HOME/.config/fani/env` with mode `0600`, and add every configured repository parent to the service's `ReadWritePaths`. Run the exact `ExecStart` command manually first.

## Maintainer checks

Run the locked quality and supply-chain gates before tagging:

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

The checked-in workflow contains deliberate least-privilege, deterministic-packaging, and smoke-test hardening beyond cargo-dist's generated defaults, so review cargo-dist updates carefully instead of overwriting the workflow without reapplying those constraints. `/workspace` must be an unmounted directory and the verifier needs non-interactive `sudo` permission for a temporary bind mount. The harness removes the mount on success, interruption, or failure.
