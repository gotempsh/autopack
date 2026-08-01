#!/usr/bin/env bash
#
# Build and run autopack against an external corpus of real applications.
#
# The examples in this repository were written to exercise autopack, so they
# are a poor judge of it. This points the same pipeline at applications written
# by someone else, for a different builder, and reports what happens.
#
#   ./scripts/sweep-external.sh path/to/temps-examples/examples/starters
#
set -uo pipefail

ROOT="${1:?usage: sweep-external.sh <corpus-dir>}"
cd "$(dirname "$0")/.." || exit 1
AUTOPACK="${AUTOPACK:-./target/debug/autopack}"
HOST_PORT="${HOST_PORT:-18997}"
CONTAINER="autopack-sweep"

[ -x "${AUTOPACK}" ] || cargo build || exit 1

cleanup() { docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

printf '%-24s %-9s %-8s %-7s %s\n' APP PROVIDER BUILD SERVES NOTE
printf '%s\n' "----------------------------------------------------------------------"

# Only directories that actually contain a manifest; anything else is not an
# application and reporting it as a failure would be noise.
apps=$(cd "${ROOT}" && find . -mindepth 1 -maxdepth 3 \
  \( -name package.json -o -name go.mod -o -name Cargo.toml -o -name requirements.txt \
     -o -name pyproject.toml -o -name composer.json -o -name Gemfile -o -name mix.exs \
     -o -name pom.xml -o -name build.gradle -o -name 'Package.swift' -o -name deno.json \
     -o -name '*.csproj' \) \
  -not -path '*/node_modules/*' -not -path '*/vendor/*' 2>/dev/null \
  | sed 's|/[^/]*$||' | sed 's|^\./||' | sort -u)

for app in ${apps}; do
  dir="${ROOT}/${app}"
  tag="autopack-sweep-$(echo "${app}" | tr '/' '-'):test"

  info=$("${AUTOPACK}" info "${dir}" 2>&1)
  if ! echo "${info}" | grep -q '^Provider'; then
    printf '%-24s %-9s %-8s %-7s %s\n' "${app}" "-" "-" "-" \
      "not detected: $(echo "${info}" | head -1 | cut -c8-46)"
    continue
  fi
  provider=$(echo "${info}" | grep '^Provider' | awk '{print $2}')

  if ! "${AUTOPACK}" build "${dir}" -t "${tag}" >/tmp/sweep-build.log 2>&1; then
    note=$(grep -oE 'ERROR: .*' /tmp/sweep-build.log | tail -1 | cut -c1-44)
    printf '%-24s %-9s %-8s %-7s %s\n' "${app}" "${provider}" "FAIL" "-" "${note}"
    continue
  fi

  cleanup
  served="no"
  for port in 3000 8000 8080; do
    docker run -d --name "${CONTAINER}" -e PORT="${port}" \
      -p "${HOST_PORT}:${port}" "${tag}" >/dev/null 2>&1 || continue
    for _ in $(seq 1 20); do
      if curl -s --max-time 2 -o /dev/null "http://127.0.0.1:${HOST_PORT}/"; then
        served="yes"
        break
      fi
      sleep 1
    done
    [ "${served}" = "yes" ] && break
    cleanup
  done

  size=$(docker image inspect "${tag}" --format '{{.Size}}' 2>/dev/null || echo 0)
  uid=$(docker run --rm --entrypoint id "${tag}" -u 2>/dev/null || echo "?")
  cleanup

  printf '%-24s %-9s %-8s %-7s %s\n' "${app}" "${provider}" "ok" "${served}" \
    "$((size / 1000000))MB uid=${uid}"
done
