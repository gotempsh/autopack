#!/usr/bin/env bash
#
# Build every example app and check it actually runs.
#
# The test suite proves a plan renders; this proves the render survives contact
# with BuildKit and that the resulting container answers.
#
#   ./scripts/verify-examples.sh                 # all examples
#   ./scripts/verify-examples.sh rails-app       # just these
#
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

AUTOPACK="${AUTOPACK:-./target/debug/autopack}"
HOST_PORT="${HOST_PORT:-18999}"
CONTAINER="autopack-verify"

# example:container-port. An empty port means the app is expected to run to
# completion rather than listen.
ALL_EXAMPLES=(
  "node-express:3000"
  "vite-spa:3000"
  "bun-server:3000"
  "deno-api:3000"
  "python-flask:8000"
  "ruby-rack:3000"
  "rails-app:3000"
  "php-app:3000"
  "php-composer:3000"
  "java-maven:3000"
  "dotnet-api:8080"
  "elixir-release:3000"
  "elixir-plug:3000"
  "gleam-cli:"
  "go-api:3000"
  "rust-api:3000"
  "haskell-api:3000"
  "cpp-cmake:3000"
  "procfile-app:3000"
  "static-site:3000"
)

if [ ! -x "${AUTOPACK}" ]; then
  echo "building autopack..."
  cargo build || exit 1
fi

selected=()
if [ "$#" -gt 0 ]; then
  for want in "$@"; do
    for entry in "${ALL_EXAMPLES[@]}"; do
      if [ "${entry%%:*}" = "${want}" ]; then
        selected+=("${entry}")
      fi
    done
  done
  if [ "${#selected[@]}" -eq 0 ]; then
    echo "no example matched: $*" >&2
    exit 2
  fi
else
  selected=("${ALL_EXAMPLES[@]}")
fi

passed=()
failed=()

cleanup() {
  docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

for entry in "${selected[@]}"; do
  example="${entry%%:*}"
  port="${entry##*:}"
  tag="autopack-${example}:verify"

  printf '\n=== %s\n' "${example}"

  if ! "${AUTOPACK}" build "examples/${example}" -t "${tag}" >/tmp/autopack-verify.log 2>&1; then
    echo "BUILD FAILED"
    tail -20 /tmp/autopack-verify.log
    failed+=("${example} (build)")
    continue
  fi

  cleanup

  # No port: the example is a one-shot program, so a clean exit is the check.
  if [ -z "${port}" ]; then
    if output=$(docker run --rm --name "${CONTAINER}" "${tag}" 2>&1); then
      echo "ran and exited 0: ${output}"
      passed+=("${example}")
    else
      echo "RUN FAILED"
      echo "${output}"
      failed+=("${example} (run)")
    fi
    continue
  fi

  if ! docker run -d --name "${CONTAINER}" -p "${HOST_PORT}:${port}" "${tag}" >/dev/null 2>&1; then
    echo "RUN FAILED (container would not start)"
    failed+=("${example} (start)")
    continue
  fi

  answered=false
  for _ in $(seq 1 45); do
    if curl -sf --max-time 3 -o /tmp/autopack-verify-body.txt "http://127.0.0.1:${HOST_PORT}/"; then
      answered=true
      break
    fi
    sleep 1
  done

  if [ "${answered}" = true ]; then
    echo "served: $(head -c 60 /tmp/autopack-verify-body.txt | tr -d '\n')"
    passed+=("${example}")
  else
    echo "NO RESPONSE. Container logs:"
    docker logs "${CONTAINER}" 2>&1 | tail -20
    failed+=("${example} (http)")
  fi

  cleanup
done

printf '\n----------------------------------------\n'
printf 'passed: %d\n' "${#passed[@]}"
if [ "${#failed[@]}" -gt 0 ]; then
  printf 'failed: %d\n' "${#failed[@]}"
  for entry in "${failed[@]}"; do
    printf '  - %s\n' "${entry}"
  done
  exit 1
fi
echo "all examples verified"
