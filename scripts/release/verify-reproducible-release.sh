#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
    echo "usage: $0 [CLEAN_SOURCE_CHECKOUT]" >&2
    exit 2
fi

source_repo=${1:-.}
source_repo=$(CDPATH= cd -- "$source_repo" && pwd)
if [ -n "$(git -C "$source_repo" status --porcelain=v1)" ]; then
    echo "reproducibility verification requires a clean source checkout" >&2
    exit 1
fi

for command in cargo dist git mount mountpoint readelf sha256sum stat sudo tar umount xz; do
    command -v "$command" >/dev/null || {
        echo "missing required command: $command" >&2
        exit 1
    }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

for environment in environment-a environment-b; do
    clone="$tmp/$environment"
    git clone --quiet --local --no-hardlinks "$source_repo" "$clone"
    test -z "$(git -C "$clone" status --porcelain=v1)"
    "$clone/scripts/release/build-reproducible-release.sh" \
        local target/distrib/local-command-manifest.json
    "$clone/scripts/release/build-reproducible-release.sh" \
        global target/distrib/global-command-manifest.json
    test -z "$(git -C "$clone" status --porcelain=v1)"
done

left="$tmp/environment-a/target/distrib"
right="$tmp/environment-b/target/distrib"
archive=fani-x86_64-unknown-linux-gnu.tar.xz

for asset in \
    fani-installer.sh \
    "$archive" \
    "$archive.sha256" \
    fani.cdx.xml \
    sha256.sum
do
    cmp "$left/$asset" "$right/$asset"
done

for environment in environment-a environment-b; do
    clone="$tmp/$environment"
    extracted="$clone/extracted"
    mkdir "$extracted"
    tar -xJf "$clone/target/distrib/$archive" -C "$extracted"
    root_dir="$extracted/fani-x86_64-unknown-linux-gnu"
    (
        cd "$extracted"
        find fani-x86_64-unknown-linux-gnu -printf '%P\t%y\t%m\t%U\t%G\t%s\t%T@\t%l\n' |
            LC_ALL=C sort
    ) > "$clone/extracted-metadata.txt"
    sha256sum "$root_dir/fani" | awk '{print $1}' > "$clone/binary-sha256.txt"
    readelf -n "$root_dir/fani" | sed -n 's/^.*Build ID: /Build ID: /p' > "$clone/build-id.txt"
    find "$root_dir" -type f ! -name fani -printf '%P\n' |
        LC_ALL=C sort |
        while IFS= read -r file; do
            sha256sum "$root_dir/$file" | sed "s|  $root_dir/|  |"
        done > "$clone/packaged-assets-sha256.txt"
done

for comparison in \
    extracted-metadata.txt \
    binary-sha256.txt \
    build-id.txt \
    packaged-assets-sha256.txt
do
    cmp "$tmp/environment-a/$comparison" "$tmp/environment-b/$comparison"
done

installer_sha256=$(sha256sum "$left/fani-installer.sh" | awk '{print $1}')
archive_sha256=$(sha256sum "$left/$archive" | awk '{print $1}')
binary_sha256=$(cat "$tmp/environment-a/binary-sha256.txt")
build_id=$(cut -d ' ' -f 3 "$tmp/environment-a/build-id.txt")
sbom_sha256=$(sha256sum "$left/fani.cdx.xml" | awk '{print $1}')
printf 'installer_sha256=%s\n' "$installer_sha256"
printf 'archive_sha256=%s\n' "$archive_sha256"
printf 'binary_sha256=%s\n' "$binary_sha256"
printf 'build_id=%s\n' "$build_id"
printf 'sbom_sha256=%s\n' "$sbom_sha256"
printf 'reproducible_release=verified\n'
