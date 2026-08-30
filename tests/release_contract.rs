use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::{TempDir, tempdir};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn output(command: &mut Command) -> Output {
    command.output().unwrap()
}

fn checked(command: &mut Command) -> Output {
    let output = output(command);
    assert!(
        output.status.success(),
        "command failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn copy(root: &Path, stage: &Path, source: &str, destination: &str) {
    fs::copy(root.join(source), stage.join(destination)).unwrap();
}

struct ArchiveFixture {
    _temp: TempDir,
    archive: PathBuf,
    sbom: PathBuf,
    stage: PathBuf,
}

impl ArchiveFixture {
    fn new() -> Self {
        let repository = root();
        let temp = tempdir().unwrap();
        let stage = temp.path().join("fani-x86_64-unknown-linux-gnu");
        fs::create_dir(&stage).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_fani"), stage.join("fani")).unwrap();
        for (source, destination) in [
            ("README.md", "README.md"),
            ("examples/fani.toml", "fani.toml"),
            ("release/completions/_fani", "_fani"),
            ("release/completions/fani.bash", "fani.bash"),
            ("release/completions/fani.fish", "fani.fish"),
            ("release/man/fani.1", "fani.1"),
            ("systemd/fani.service", "fani.service"),
            ("systemd/fani.timer", "fani.timer"),
        ] {
            copy(&repository, &stage, source, destination);
        }

        let archive = temp.path().join("fani-x86_64-unknown-linux-gnu.tar.xz");
        checked(
            Command::new("tar")
                .current_dir(temp.path())
                .args([
                    "--sort=name",
                    "--mtime=@0",
                    "--owner=0",
                    "--group=0",
                    "--numeric-owner",
                    "-cJf",
                ])
                .arg(&archive)
                .arg(stage.file_name().unwrap()),
        );
        let checksum = checked(Command::new("sha256sum").arg(&archive));
        let hash = String::from_utf8(checksum.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        fs::write(
            format!("{}.sha256", archive.display()),
            format!(
                "{hash} *{}\n",
                archive.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let sbom = temp.path().join("fani.cdx.xml");
        fs::write(
            &sbom,
            r#"<?xml version="1.0"?><bom xmlns="http://cyclonedx.org/schema/bom/1.6"><metadata><component><name>fani</name><hashes><hash alg="SHA-256">00</hash></hashes></component></metadata></bom>"#,
        )
        .unwrap();

        Self {
            _temp: temp,
            archive,
            sbom,
            stage,
        }
    }

    fn verify(&self) -> Output {
        output(
            Command::new(root().join("scripts/release/verify-archive.sh"))
                .arg(&self.archive)
                .arg(&self.sbom),
        )
    }

    fn rebuild(&self) {
        checked(
            Command::new("tar")
                .current_dir(self.stage.parent().unwrap())
                .args([
                    "--sort=name",
                    "--mtime=@0",
                    "--owner=0",
                    "--group=0",
                    "--numeric-owner",
                    "-cJf",
                ])
                .arg(&self.archive)
                .arg(self.stage.file_name().unwrap()),
        );
        let checksum = checked(Command::new("sha256sum").arg(&self.archive));
        let hash = String::from_utf8(checksum.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        fs::write(
            format!("{}.sha256", self.archive.display()),
            format!(
                "{hash} *{}\n",
                self.archive.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
    }
}

#[test]
fn cargo_dist_is_linux_gnu_only_and_pins_release_inputs() {
    let toolchain = fs::read_to_string(root().join("rust-toolchain.toml")).unwrap();
    for required in [
        "channel = \"1.85.0\"",
        "components = [\"clippy\", \"rustfmt\"]",
        "targets = [\"x86_64-unknown-linux-gnu\"]",
    ] {
        assert!(toolchain.contains(required), "missing {required}");
    }

    let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
    for required in [
        "cargo-dist-version = \"0.32.0\"",
        "targets = [\"x86_64-unknown-linux-gnu\"]",
        "checksum = \"sha256\"",
        "source-tarball = false",
        "cargo-cyclonedx = true",
        "github-attestations = true",
        "github-attestations-phase = \"host\"",
        "release/completions/fani.bash",
        "release/completions/_fani",
        "release/completions/fani.fish",
        "release/man/fani.1",
        "systemd/fani.service",
        "systemd/fani.timer",
        "examples/fani.toml",
    ] {
        assert!(manifest.contains(required), "missing {required}");
    }
    for forbidden in [
        "unknown-linux-musl",
        "apple-darwin",
        "pc-windows",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(!manifest.contains(forbidden), "found {forbidden}");
    }
}

#[test]
fn release_workflow_has_least_privilege_pinned_provenance_and_gates() {
    let workflows = root().join(".github/workflows");
    let release = fs::read_to_string(workflows.join("release.yml")).unwrap();
    for required in [
        "permissions: {}",
        "actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6",
        "\"attestations\": \"write\"",
        "\"id-token\": \"write\"",
        "subject-path: |\n            artifacts/*",
        "cargo install --locked cargo-cyclonedx@0.5.9",
        "scripts/release/install-reproducible-dist.sh",
        "scripts/release/build-reproducible-release.sh local",
        "scripts/release/build-reproducible-release.sh global",
        "scripts/release/verify-archive.sh",
        "steps.cargo-cyclonedx.outputs.paths",
        "persist-credentials: false",
    ] {
        assert!(release.contains(required), "missing {required}");
    }
    assert_eq!(release.matches("\"id-token\": \"write\"").count(), 1);
    assert_eq!(release.matches("\"attestations\": \"write\"").count(), 1);
    assert!(!release.contains("ubuntu-latest"));
    assert!(!release.contains("actions/cache"));

    for script in [
        "install-reproducible-dist.sh",
        "build-reproducible-release.sh",
        "verify-reproducible-release.sh",
    ] {
        let source = fs::read_to_string(root().join("scripts/release").join(script)).unwrap();
        assert!(source.starts_with("#!/bin/sh\n"));
        assert!(!source.contains("BASH_SOURCE"), "{script}");
        assert!(!source.contains("pipefail"), "{script}");
        assert!(!source.contains("python"), "{script}");
        assert!(!source.contains("uv "), "{script}");
    }
    let installer =
        fs::read_to_string(root().join("scripts/release/install-reproducible-dist.sh")).unwrap();
    for required in [
        "cargo_dist_version=0.32.0",
        "axoasset_version=2.0.1",
        "tar.mode(tar::HeaderMode::Deterministic);",
        "cargo +1.98.0 install --locked --jobs 2 --path",
    ] {
        assert!(installer.contains(required), "missing {required}");
    }
    let verifier =
        fs::read_to_string(root().join("scripts/release/verify-reproducible-release.sh")).unwrap();
    for required in [
        "git clone --quiet --local --no-hardlinks",
        "fani-x86_64-unknown-linux-gnu.tar.xz",
        "fani.cdx.xml",
        "binary-sha256.txt",
        "build-id.txt",
        "packaged-assets-sha256.txt",
        "extracted-metadata.txt",
    ] {
        assert!(verifier.contains(required), "missing {required}");
    }

    for line in release
        .lines()
        .filter(|line| line.trim().starts_with("uses:"))
    {
        let action_ref = line.split_once('@').unwrap().1.trim();
        assert_eq!(action_ref.len(), 40, "unfixed action: {line}");
        assert!(action_ref.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    let security = fs::read_to_string(workflows.join("security.yml")).unwrap();
    for required in [
        "pull_request:",
        "branches: [main]",
        "cargo install --locked cargo-audit@0.22.2 cargo-deny@0.20.2",
        "run: cargo audit",
        "run: cargo deny check",
    ] {
        assert!(security.contains(required), "missing {required}");
    }
}

#[test]
fn archive_verifier_accepts_allowlisted_assets_and_smokes_the_binary() {
    let fixture = ArchiveFixture::new();
    let verified = fixture.verify();
    assert!(
        verified.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&verified.stdout),
        String::from_utf8_lossy(&verified.stderr)
    );
}

#[test]
fn archive_verifier_rejects_runtime_and_work_data() {
    let fixture = ArchiveFixture::new();
    fs::create_dir_all(fixture.stage.join("reports")).unwrap();
    fs::write(fixture.stage.join("reports/report.json"), "{}\n").unwrap();
    fixture.rebuild();
    let rejected = fixture.verify();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("allowlist"));
}
