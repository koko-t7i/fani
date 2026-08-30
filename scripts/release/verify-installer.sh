#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 INSTALLER ARCHIVE" >&2
    exit 2
fi

installer=$1
archive=$2

for command in grep sha256sum sh; do
    command -v "$command" >/dev/null || {
        echo "missing required command: $command" >&2
        exit 1
    }
done

[ -f "$installer" ] || {
    echo "installer not found: $installer" >&2
    exit 1
}
[ -f "$archive" ] || {
    echo "archive not found: $archive" >&2
    exit 1
}

sh -n "$installer"
archive_sha256=$(sha256sum "$archive" | awk '{print $1}')

grep -F '_checksum_style="sha256"' "$installer" >/dev/null
grep -F "_checksum_value=\"$archive_sha256\"" "$installer" >/dev/null
grep -F 'https://github.com/koko-t7i/fani/releases/download/' "$installer" >/dev/null
grep -F '_install_dir="$INFERRED_HOME/.local/bin"' "$installer" >/dev/null

if grep -F 'github.com/koko/fani' "$installer" >/dev/null; then
    echo "installer contains the obsolete repository URL" >&2
    exit 1
fi

printf 'installer_sha256=%s\n' "$(sha256sum "$installer" | awk '{print $1}')"
printf 'embedded_archive_sha256=%s\n' "$archive_sha256"
printf 'installer=verified\n'
