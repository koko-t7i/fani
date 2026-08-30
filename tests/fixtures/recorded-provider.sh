#!/bin/sh
set -eu

[ "$#" -eq 1 ] || exit 41
[ -n "${HOME:-}" ] || exit 42
[ -d "$HOME" ] || exit 43
[ -z "${CARGO_HOME+x}" ] || exit 44
[ -z "${RUSTUP_HOME+x}" ] || exit 45
[ -z "${XDG_CACHE_HOME+x}" ] || exit 46
[ -z "${XDG_CONFIG_HOME+x}" ] || exit 47
[ -z "${ANTHROPIC_API_KEY+x}" ] || exit 48
[ -z "${OPENAI_API_KEY+x}" ] || exit 49
[ -z "${AWS_ACCESS_KEY_ID+x}" ] || exit 50
[ -z "${GITHUB_TOKEN+x}" ] || exit 51
[ -z "${GH_TOKEN+x}" ] || exit 52
[ -z "${HTTP_PROXY+x}" ] || exit 53
[ -z "${HTTPS_PROXY+x}" ] || exit 54
[ -z "${ALL_PROXY+x}" ] || exit 55

request=''
IFS= read -r request || [ -n "$request" ] || exit 56
case "$request" in
  *'"schema":"fani.agent.request.v1"'*'"source":"Deterministic source bytes."'*) ;;
  *) exit 64 ;;
esac
remainder=${request#*\"id\":\"}
[ "$remainder" != "$request" ] || exit 57
task_id=${remainder%%\"*}
[ -n "$task_id" ] || exit 58

IFS= read -r fixture < "$1" || exit 59
[ "$fixture" = '{"schema":"fani.recorded-provider-fixture.v1","source":"Deterministic source bytes.","output":"Octets source deterministes."}' ] || exit 60
printf '{"schema":"fani.agent.response.v1","task_id":"%s","output":"Octets source deterministes."}\n' "$task_id"
