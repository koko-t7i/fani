#!/bin/sh
set -eu

if [ "$#" -lt 1 ]; then
    echo "usage: $0 local|global [DIST_MANIFEST] [DIST_ARGS...]" >&2
    exit 2
fi

mode=$1
shift
manifest=${1:-dist-manifest.json}
if [ "$#" -gt 0 ]; then
    shift
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"
umask 022

case "$mode" in
    local|global) ;;
    *)
        echo "mode must be local or global" >&2
        exit 2
        ;;
esac

if [ -n "$(git status --porcelain=v1)" ]; then
    echo "release build requires a clean checkout" >&2
    exit 1
fi

rustc --version | grep -Eq '^rustc 1\.85\.0 '
dist --version | grep -Fx 'cargo-dist 0.32.0'
source_date_epoch=$(git show -s --format=%ct HEAD)
export SOURCE_DATE_EPOCH=$source_date_epoch
mkdir -p "$(dirname -- "$manifest")"

if [ "$mode" = local ]; then
    dist build --artifacts=local --output-format=json "$@" > "$manifest"
    mkdir -p target/distrib
    cp "$manifest" target/distrib/x86_64-unknown-linux-gnu-dist-manifest.json
    exit 0
fi

cargo cyclonedx --version | grep -Fx 'cargo-cyclonedx-cyclonedx 0.5.9'
for command in mount mountpoint sudo umount; do
    command -v "$command" >/dev/null
done
if [ ! -d /workspace ]; then
    echo "stable SBOM mount point /workspace is missing" >&2
    exit 1
fi
if mountpoint -q /workspace; then
    echo "stable SBOM mount point /workspace is already in use" >&2
    exit 1
fi

dist build --artifacts=global --output-format=json "$@" > "$manifest"
sudo mount --bind "$root" /workspace
trap 'cd /; sudo umount /workspace' EXIT HUP INT TERM
(
    cd /workspace
    cargo cyclonedx -q
    mv fani.cdx.xml target/distrib/fani.cdx.xml
)
sudo umount /workspace
trap - EXIT HUP INT TERM

scripts/release/verify-archive.sh \
    target/distrib/fani-x86_64-unknown-linux-gnu.tar.xz \
    target/distrib/fani.cdx.xml
