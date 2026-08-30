#!/bin/sh
set -eu

: "${GITHUB_EVENT_NAME:?GITHUB_EVENT_NAME is required}"
: "${GITHUB_REF:?GITHUB_REF is required}"
: "${GITHUB_DEFAULT_BRANCH:?GITHUB_DEFAULT_BRANCH is required}"

expected_ref="refs/heads/${GITHUB_DEFAULT_BRANCH}"
case "${GITHUB_EVENT_NAME}" in
    push|schedule|workflow_dispatch)
        ;;
    *)
        echo "provider access denied for event ${GITHUB_EVENT_NAME}" >&2
        exit 1
        ;;
esac

if [ "${GITHUB_REF}" != "${expected_ref}" ]; then
    echo "provider access denied for ref ${GITHUB_REF}" >&2
    exit 1
fi
