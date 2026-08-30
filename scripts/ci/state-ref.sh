#!/bin/sh
set -eu

usage() {
    echo "usage: state-ref.sh restore|publish" >&2
    exit 2
}

[ "$#" -eq 1 ] || usage
command_name="$1"

repo_root="$(git rev-parse --show-toplevel)"
remote="${FANI_STATE_REMOTE:-origin}"
state_ref="${FANI_STATE_REF:-refs/heads/fani-state}"
db_path="${FANI_STATE_DB:-.fani/fani.db}"
expected_file="$(git rev-parse --git-path fani-state-expected)"
zero_oid="0000000000000000000000000000000000000000"
tmp=""

cleanup() {
    if [ -n "${tmp}" ]; then
        rm -f "${tmp}"
    fi
}
trap cleanup EXIT HUP INT TERM

git_remote() {
    if [ -n "${FANI_STATE_TOKEN:-}" ]; then
        encoded="$(printf 'x-access-token:%s' "${FANI_STATE_TOKEN}" | base64 | tr -d '\n')"
        git -c "http.https://github.com/.extraheader=AUTHORIZATION: basic ${encoded}" "$@"
    else
        git "$@"
    fi
}

case "${state_ref}" in
    refs/heads/*) ;;
    *)
        echo "FANI_STATE_REF must be a dedicated branch ref under refs/heads/" >&2
        exit 2
        ;;
esac
case "${db_path}" in
    /*|../*|*/../*)
        echo "FANI_STATE_DB must stay inside the repository" >&2
        exit 2
        ;;
esac

validate_db() {
    validation_path="$1"
    : "${FANI_BIN:?FANI_BIN is required}"
    : "${FANI_CONFIG_PATH:?FANI_CONFIG_PATH is required}"
    for suffix in -journal -shm -wal; do
        if [ -e "${validation_path}${suffix}" ]; then
            echo "refusing to persist SQLite sidecar ${validation_path}${suffix}" >&2
            return 1
        fi
    done
    "${FANI_BIN}" doctor --config "${FANI_CONFIG_PATH}" >/dev/null
    for suffix in -journal -shm -wal; do
        if [ -e "${validation_path}${suffix}" ]; then
            echo "refusing to persist SQLite sidecar ${validation_path}${suffix}" >&2
            return 1
        fi
    done
}

write_output() {
    output_oid="$1"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        printf 'state_oid=%s\n' "${output_oid}" >> "${GITHUB_OUTPUT}"
    fi
    printf '%s\n' "${output_oid}"
}

restore() {
    fetched_ref="refs/fani-state/fetched"
    remote_line="$(git_remote ls-remote --refs "${remote}" "${state_ref}")"
    if [ -z "${remote_line}" ]; then
        printf '%s\n' "${zero_oid}" > "${expected_file}"
        write_output "${zero_oid}"
        return 0
    fi

    expected_oid="${remote_line%%[[:space:]]*}"
    git_remote fetch --no-tags --force "${remote}" "+${state_ref}:${fetched_ref}"
    if [ "$(git rev-parse "${fetched_ref}^{commit}")" != "${expected_oid}" ]; then
        echo "state ref changed while it was fetched; retry the workflow" >&2
        exit 1
    fi

    tree_entries="$(git ls-tree -r --name-only "${expected_oid}")"
    if [ "${tree_entries}" != "fani.db" ]; then
        echo "state ref must contain exactly fani.db" >&2
        exit 1
    fi

    mkdir -p "${repo_root}/$(dirname "${db_path}")"
    tmp="$(mktemp "${repo_root}/${db_path}.restore.XXXXXX")"
    git show "${expected_oid}:fani.db" > "${tmp}"
    chmod 600 "${tmp}"
    mv -f "${tmp}" "${repo_root}/${db_path}"
    tmp=""
    validate_db "${repo_root}/${db_path}"
    printf '%s\n' "${expected_oid}" > "${expected_file}"
    write_output "${expected_oid}"
}

publish() {
    if [ ! -f "${expected_file}" ]; then
        echo "state restore metadata is missing; restore before publish" >&2
        exit 1
    fi
    expected_oid="$(cat "${expected_file}")"
    case "${expected_oid}" in
        *[!0-9a-f]*)
            echo "state restore metadata is invalid" >&2
            exit 1
            ;;
    esac
    if [ "${#expected_oid}" -ne 40 ]; then
        echo "state restore metadata is invalid" >&2
        exit 1
    fi

    validate_db "${repo_root}/${db_path}"
    blob_oid="$(git hash-object -w "${repo_root}/${db_path}")"
    tree_oid="$(printf '100600 blob %s\tfani.db\n' "${blob_oid}" | git mktree)"
    if [ "${expected_oid}" = "${zero_oid}" ]; then
        commit_oid="$(
            GIT_AUTHOR_NAME="fani state" \
            GIT_AUTHOR_EMAIL="fani-state@users.noreply.github.com" \
            GIT_COMMITTER_NAME="fani state" \
            GIT_COMMITTER_EMAIL="fani-state@users.noreply.github.com" \
            git commit-tree "${tree_oid}" -m "Persist fani CI state"
        )"
    else
        commit_oid="$(
            GIT_AUTHOR_NAME="fani state" \
            GIT_AUTHOR_EMAIL="fani-state@users.noreply.github.com" \
            GIT_COMMITTER_NAME="fani state" \
            GIT_COMMITTER_EMAIL="fani-state@users.noreply.github.com" \
            git commit-tree "${tree_oid}" -p "${expected_oid}" -m "Persist fani CI state"
        )"
    fi

    git_remote push --porcelain "${remote}" \
        "${commit_oid}:${state_ref}" \
        "--force-with-lease=${state_ref}:${expected_oid}"
    printf '%s\n' "${commit_oid}" > "${expected_file}"
    write_output "${commit_oid}"
}

case "${command_name}" in
    restore) restore ;;
    publish) publish ;;
    *) usage ;;
esac
