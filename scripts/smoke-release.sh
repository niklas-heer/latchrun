#!/bin/sh
# Exercise the packaged executable without accessing any real credentials.
set -eu
umask 077
test "$#" -eq 1
binary=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")
fixture=$(mktemp -d /tmp/latchrun-release.XXXXXX)
service_pid=
cleanup() {
  if test -n "$service_pid"; then
    kill "$service_pid" 2>/dev/null || true
    count=0
    while kill -0 "$service_pid" 2>/dev/null && test "$count" -lt 100; do
      sleep 0.05
      count=$((count + 1))
    done
    kill -KILL "$service_pid" 2>/dev/null || true
    wait "$service_pid" 2>/dev/null || true
  fi
  rm -rf "$fixture"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
mkdir "$fixture/runtime" "$fixture/data" "$fixture/project"
cli() { "$binary" --runtime-dir "$fixture/runtime" --data-dir "$fixture/data" "$@"; }
"$binary" --runtime-dir "$fixture/runtime" --data-dir "$fixture/data" service serve >"$fixture/service.log" 2>&1 &
service_pid=$!
count=0
until cli service status >/dev/null 2>&1; do
  kill -0 "$service_pid"
  test "$count" -lt 100
  sleep 0.05
  count=$((count + 1))
done
printf '{"project":"%s/project","purpose":"release smoke test","provider":"fake","credentials":{"TOKEN":"fake://release-smoke"},"commands":[{"executable":"/usr/bin/printenv","args":["TOKEN"]}]}\n' "$fixture" >"$fixture/profile.json"
cli session start smoke --profile "$fixture/profile.json" >/dev/null
output=$(cli run smoke --operation release-smoke -- /usr/bin/printenv TOKEN)
test "$output" = '[REDACTED]'
cli stats | grep -Eq '"succeeded":[[:space:]]*1'
cli service stop >/dev/null
wait "$service_pid"
service_pid=
printf '%s\n' 'Packaged executable passed fake-provider lifecycle and redaction smoke test.'
