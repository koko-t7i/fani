#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 ARCHIVE SBOM" >&2
    exit 2
fi

archive=$1
sbom=$2
checksum="${archive}.sha256"

for file in "$archive" "$checksum" "$sbom"; do
    if [ ! -f "$file" ]; then
        echo "missing release asset: $file" >&2
        exit 1
    fi
done

case "$archive" in
    *.tar.xz) ;;
    *)
        echo "release archive must be .tar.xz: $archive" >&2
        exit 1
        ;;
esac

case "$sbom" in
    *.cdx.xml) ;;
    *)
        echo "SBOM must be CycloneDX XML: $sbom" >&2
        exit 1
        ;;
esac

grep -q '<bom ' "$sbom"
grep -q 'name="fani"\|<name>fani</name>' "$sbom"
grep -q '<hash alg=' "$sbom"

archive_dir=$(CDPATH= cd -- "$(dirname -- "$archive")" && pwd)
archive_name=$(basename -- "$archive")
(
    cd "$archive_dir"
    sha256sum --check --strict "${archive_name}.sha256"
)

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

tar -xJf "$archive" -C "$tmp"
root_count=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
if [ "$root_count" -ne 1 ]; then
    echo "archive must contain one top-level directory" >&2
    exit 1
fi
root_dir=$(find "$tmp" -mindepth 1 -maxdepth 1 -type d)

find "$root_dir" -type f -printf '%P\n' | LC_ALL=C sort > "$tmp/actual"
cat > "$tmp/expected" <<'CONTENTS'
README.md
_fani
fani
fani.1
fani.bash
fani.fish
fani.service
fani.timer
fani.toml
CONTENTS

if ! diff -u "$tmp/expected" "$tmp/actual"; then
    echo "release archive contents differ from the allowlist" >&2
    exit 1
fi

if grep -Eiq '(^|/)(fani\.db|\.fani|prompts?|responses?|reports?|work|work-files?)(/|$)' "$tmp/actual"; then
    echo "release archive contains private runtime or work data" >&2
    exit 1
fi

binary="$root_dir/fani"
if [ ! -x "$binary" ]; then
    echo "release binary is missing or not executable" >&2
    exit 1
fi
if ! file "$binary" | grep -q 'ELF 64-bit.*x86-64'; then
    echo "release binary is not x86_64 Linux ELF" >&2
    exit 1
fi
"$binary" --version | grep -Fx "fani 0.3.0"
"$binary" --help | grep -q 'Native continuous Markdown translation'
