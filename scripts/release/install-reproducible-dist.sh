#!/bin/sh
set -eu

cargo_dist_version=0.32.0
axoasset_version=2.0.1
cargo_dist_sha256=875d63e07ac553562b350004fea408da40cf64b565148234e513186d73f5e70f
axoasset_sha256=1be1b9c2739b635e04c7bbcde9e89dd5e874b9e86e28f1b41c44eb830635d83e

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
stage="$root/target/reproducible-dist-src"
rm -rf "$stage"
mkdir -p "$stage"

fetch_crate() {
    name=$1
    version=$2
    expected=$3
    archive="$stage/$name-$version.crate"
    curl --proto '=https' --tlsv1.2 -LsSf \
        "https://static.crates.io/crates/$name/$name-$version.crate" \
        -o "$archive"
    printf '%s  %s\n' "$expected" "$archive" | sha256sum --check --strict
    tar -xzf "$archive" -C "$stage"
}

fetch_crate cargo-dist "$cargo_dist_version" "$cargo_dist_sha256"
fetch_crate axoasset "$axoasset_version" "$axoasset_sha256"

compression="$stage/axoasset-$axoasset_version/src/compression.rs"
count=$(grep -c 'let mut tar = tar::Builder::new(zip_output);' "$compression")
if [ "$count" -ne 3 ]; then
    echo "unexpected axoasset tar builder count: $count" >&2
    exit 1
fi
sed -i '/let mut tar = tar::Builder::new(zip_output);/a\            tar.mode(tar::HeaderMode::Deterministic);' "$compression"

manifest="$stage/cargo-dist-$cargo_dist_version/Cargo.toml"
printf '\n[workspace]\n' >> "$manifest"
sed -i '/^\[dependencies.axoasset\]$/,/^\[/ {
    /^version = "2\.0\.0"$/a\path = "../axoasset-2.0.1"
}' "$manifest"

lock="$stage/cargo-dist-$cargo_dist_version/Cargo.lock"
lock_tmp="$lock.tmp"
awk '
    /^\[\[package\]\]$/ { in_axoasset = 0 }
    /^name = "axoasset"$/ { in_axoasset = 1 }
    in_axoasset && /^(source|checksum) = / { next }
    { print }
' "$lock" > "$lock_tmp"
mv "$lock_tmp" "$lock"

cargo +1.98.0 install --locked --jobs 2 --path "$stage/cargo-dist-$cargo_dist_version" --force
dist --version | grep -Fx "cargo-dist $cargo_dist_version"
