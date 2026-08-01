#!/usr/bin/env bash
#
# Build the same corpus with nixpacks, railpack and autopack and record what
# each produced. Emits TSV to stdout; everything else goes to stderr.
#
# I wrote autopack, so a self-comparison is worth distrusting by default. The
# mitigation is that every number here comes from a command anyone can re-run,
# the corpus was written by someone else for a different builder, and the
# failures are reported with the tool's own error text.
#
#   ./scripts/compare-builders.sh path/to/temps-examples/examples/starters > results.tsv
#
set -uo pipefail

CORPUS="${1:?usage: compare-builders.sh <corpus-dir>}"
cd "$(dirname "$0")/.." || exit 1
AUTOPACK="${AUTOPACK:-./target/debug/autopack}"
HOST_PORT="${HOST_PORT:-18899}"
CONTAINER="builder-cmp"
export BUILDKIT_HOST="${BUILDKIT_HOST:-docker-container://buildkit}"

APPS=(
  "go/net-http"
  "go/gin"
  "nodejs/express"
  "nodejs/fastify"
  "bun/bun-server"
  "deno"
  "python/flask"
  "python/fastapi"
  "python/django"
  "rust/actix"
  "php/vanilla"
  "java/spring-boot"
  "vite/react"
)

cleanup() { docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

printf 'app\tbuilder\tbuilt\tsecs\tsize_mb\tserves\tuid\tsigterm_s\tnote\n'

for app in "${APPS[@]}"; do
  dir="${CORPUS}/${app}"
  [ -d "${dir}" ] || continue

  for builder in nixpacks railpack autopack; do
    slug=$(echo "${app}" | tr '/' '-')
    tag="cmp-${builder}-${slug}:test"
    log=$(mktemp)

    start=$(date +%s)
    case "${builder}" in
      nixpacks) nixpacks build "${dir}" --name "${tag}" >"${log}" 2>&1 ;;
      railpack) railpack build "${dir}" --name "${tag}" >"${log}" 2>&1 ;;
      autopack) "${AUTOPACK}" build "${dir}" -t "${tag}" >"${log}" 2>&1 ;;
    esac
    status=$?
    secs=$(( $(date +%s) - start ))

    if [ "${status}" -ne 0 ]; then
      # The tool's own last error line, so the reason is attributable.
      note=$(grep -oiE '(error|ERROR)[^"]{0,70}' "${log}" | tail -1 | tr -d '\t' | cut -c1-70)
      printf '%s\t%s\tno\t%s\t-\t-\t-\t-\t%s\n' "${app}" "${builder}" "${secs}" "${note:-build failed}"
      rm -f "${log}"
      continue
    fi
    rm -f "${log}"

    size=$(docker image inspect "${tag}" --format '{{.Size}}' 2>/dev/null || echo 0)
    size_mb=$((size / 1000000))
    uid=$(docker run --rm --entrypoint id "${tag}" -u 2>/dev/null | tr -d '\r' || echo "?")
    [ -z "${uid}" ] && uid="?"

    serves="no"; sigterm="-"
    for port in 3000 8000 8080; do
      cleanup
      docker run -d --name "${CONTAINER}" -e PORT="${port}" \
        -p "${HOST_PORT}:${port}" "${tag}" >/dev/null 2>&1 || continue
      for _ in $(seq 1 25); do
        if curl -s --max-time 2 -o /dev/null "http://127.0.0.1:${HOST_PORT}/"; then
          serves="yes"
          break
        fi
        sleep 1
      done
      if [ "${serves}" = "yes" ]; then
        # How long the container takes to honour SIGTERM. 10s means it never
        # did and Docker killed it.
        t0=$(date +%s)
        docker stop -t 12 "${CONTAINER}" >/dev/null 2>&1
        sigterm=$(( $(date +%s) - t0 ))
        break
      fi
    done
    cleanup

    printf '%s\t%s\tyes\t%s\t%s\t%s\t%s\t%s\t\n' \
      "${app}" "${builder}" "${secs}" "${size_mb}" "${serves}" "${uid}" "${sigterm}"
  done
done
