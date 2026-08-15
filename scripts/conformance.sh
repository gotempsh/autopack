#!/usr/bin/env bash
#
# Runtime conformance suite.
#
# `verify-examples.sh` answers "does it build and respond". That is the floor,
# not the bar. A deployment platform also needs to know that the container
# honours $PORT, dies on SIGTERM, does not ship a compiler or a leaked secret,
# and that editing a source file does not reinstall every dependency.
#
# Each check below has failed for at least one real builder, so each one is
# here because it catches something, not because it is easy to measure.
#
#   ./scripts/conformance.sh                 # every example
#   ./scripts/conformance.sh go-api rust-api # a subset
#
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

AUTOPACK="${AUTOPACK:-./target/debug/autopack}"
PORT_A="${PORT_A:-18991}"   # default-port run
PORT_B="${PORT_B:-18992}"   # custom-$PORT run
CUSTOM_PORT=8137            # deliberately not any provider's default
CONTAINER="autopack-conf"
SECRET_SENTINEL="autopack-secret-must-not-be-in-any-layer-9f3a1c"
WORK="$(mktemp -d)"

# example : default-port : binaries that must NOT exist in the runtime image
#
# The absent-list encodes each provider's own claim. A Go image carrying `go`,
# or any image carrying `mise` after the provider said it dropped the runtime,
# is a silent several-hundred-megabyte regression.
SPECS=(
  "node-express:3000:"
  "playwright-app:3000:"
  "vite-spa:3000:mise,node,npm"
  "bun-server:3000:"
  "deno-api:3000:"
  "python-flask:8000:"
  "python-native:8000:"
  "ruby-rack:3000:"
  "ruby-native:3000:"
  "rails-app:3000:"
  "php-app:3000:"
  "php-composer:3000:composer"
  "php-extensions:3000:"
  "java-maven:3000:mise,mvn,javac"
  "scala-app:3000:mise,sbt,scalac"
  "clojure-app:3000:mise,lein"
  "dotnet-api:8080:mise,dotnet-sdk"
  "elixir-release:3000:mise,elixir,mix,iex"
  "elixir-plug:3000:mise,elixir,mix,iex"
  "gleam-cli::mise"
  "go-api:3000:mise,go,gcc,cc"
  "rust-api:3000:mise,cargo,rustc,cc"
  "lunatic-app:3000:mise,cargo,rustc"
  "haskell-api:3000:mise,ghc,cabal,stack"
  "swift-server:3000:mise,swiftc"
  "dart-server:3000:mise,dart"
  "zig-server:3000:mise,zig"
  "crystal-server:3000:"
  "cobol-app::mise,cobc"
  "cpp-cmake:3000:mise,cmake,g++,cc"
  "procfile-app:3000:mise"
  "static-site:3000:mise,node,caddy-builder"
)

pass=0
fail=0
declare -a FAILURES=()
declare -a REPORT=()

cleanup() { docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true; }
trap 'cleanup; rm -rf "${WORK}"' EXIT

ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s — %s\n' "$1" "$2"; fail=$((fail + 1)); FAILURES+=("$3: $1 — $2"); }
skip() { printf '  \033[90mn/a \033[0m %s — %s\n' "$1" "$2"; }

wait_for_http() {
  local port=$1 tries=${2:-45}
  for _ in $(seq 1 "${tries}"); do
    if curl -sf --max-time 3 -o "${WORK}/body.txt" "http://127.0.0.1:${port}/"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

selected=()
if [ "$#" -gt 0 ]; then
  for want in "$@"; do
    for spec in "${SPECS[@]}"; do
      [ "${spec%%:*}" = "${want}" ] && selected+=("${spec}")
    done
  done
  [ "${#selected[@]}" -eq 0 ] && { echo "no example matched: $*" >&2; exit 2; }
else
  selected=("${SPECS[@]}")
fi

[ -x "${AUTOPACK}" ] || cargo build || exit 1

for spec in "${selected[@]}"; do
  example="${spec%%:*}"
  rest="${spec#*:}"
  port="${rest%%:*}"
  absent="${rest#*:}"
  tag="autopack-${example}:conf"

  printf '\n\033[1m=== %s\033[0m\n' "${example}"

  # ---------------------------------------------------------------- 1. build
  if ! AUTOPACK_TEST_SECRET="${SECRET_SENTINEL}" "${AUTOPACK}" build "examples/${example}" \
        -t "${tag}" --secret AUTOPACK_TEST_SECRET >"${WORK}/build.log" 2>&1; then
    bad "build" "see log" "${example}"
    tail -15 "${WORK}/build.log" | sed 's/^/      /'
    continue
  fi
  ok "build"

  # ------------------------------------------------- 2. deterministic output
  "${AUTOPACK}" dockerfile "examples/${example}" >"${WORK}/df1" 2>/dev/null
  "${AUTOPACK}" dockerfile "examples/${example}" >"${WORK}/df2" 2>/dev/null
  if cmp -s "${WORK}/df1" "${WORK}/df2"; then
    ok "dockerfile is deterministic"
  else
    bad "dockerfile is deterministic" "two runs differ" "${example}"
  fi

  # ------------------------------------------------ 3. no secret in any layer
  # A secret mounted with --mount=type=secret must never land in a layer, even
  # a layer that a later step deletes.
  if docker save "${tag}" 2>/dev/null | grep -a -q "${SECRET_SENTINEL}"; then
    bad "no build secret in image layers" "sentinel found in a layer" "${example}"
  else
    ok "no build secret in image layers"
  fi

  # ------------------------------------------- 4. runtime image stays minimal
  if [ -n "${absent}" ]; then
    leaked=""
    IFS=',' read -ra bins <<< "${absent}"
    for bin in "${bins[@]}"; do
      [ -z "${bin}" ] && continue
      if docker run --rm --entrypoint sh "${tag}" -c \
           "command -v ${bin} >/dev/null 2>&1 || ls /mise >/dev/null 2>&1" >/dev/null 2>&1; then
        if [ "${bin}" = "mise" ]; then
          docker run --rm --entrypoint sh "${tag}" -c "ls /mise" >/dev/null 2>&1 && leaked="${leaked} ${bin}"
        else
          docker run --rm --entrypoint sh "${tag}" -c "command -v ${bin}" >/dev/null 2>&1 && leaked="${leaked} ${bin}"
        fi
      fi
    done
    if [ -n "${leaked}" ]; then
      bad "runtime image drops build tooling" "found:${leaked}" "${example}"
    else
      ok "runtime image drops build tooling"
    fi
  else
    skip "runtime image drops build tooling" "runtime legitimately needs its toolchain"
  fi

  # --------------------------------------------------------- 5. image is lean
  size_bytes=$(docker image inspect "${tag}" --format '{{.Size}}' 2>/dev/null || echo 0)
  size_mb=$((size_bytes / 1000000))
  REPORT+=("${example} ${size_mb}MB")

  # -------------------------------------------------- 6. runs unprivileged
  # Root in a container turns any escape or bad bind-mount into host-level
  # access, and is the first finding of every image scanner.
  user=$(docker image inspect "${tag}" --format '{{.Config.User}}' 2>/dev/null)
  runtime_uid=$(docker run --rm --entrypoint id "${tag}" -u 2>/dev/null || echo "?")
  REPORT+=("${example} user=${user:-root} uid=${runtime_uid}")
  if [ "${runtime_uid}" = "0" ] || [ -z "${user}" ]; then
    bad "runs unprivileged" "running as uid ${runtime_uid}" "${example}"
  else
    ok "runs unprivileged (uid ${runtime_uid})"
  fi

  # One-shot programs stop here: there is no server to interrogate.
  if [ -z "${port}" ]; then
    if output=$(docker run --rm "${tag}" 2>&1); then
      ok "runs and exits 0 (${output})"
    else
      bad "runs and exits 0" "${output}" "${example}"
    fi
    continue
  fi

  # ------------------------------------------------------- 7. serves content
  cleanup
  docker run -d --name "${CONTAINER}" -p "${PORT_A}:${port}" "${tag}" >/dev/null 2>&1
  if wait_for_http "${PORT_A}"; then
    body=$(tr -d '\n' <"${WORK}/body.txt")
    if grep -qi "hello from autopack\|<!doctype html" "${WORK}/body.txt"; then
      ok "serves expected content"
    else
      bad "serves expected content" "got: ${body:0:60}" "${example}"
    fi
  else
    bad "serves expected content" "no response on :${port}" "${example}"
    docker logs "${CONTAINER}" 2>&1 | tail -8 | sed 's/^/      /'
    cleanup
    continue
  fi

  # ------------------------------------------------- 8. SIGTERM is respected
  # A process running as PID 1 with only the default disposition never dies on
  # SIGTERM — the kernel discards it. The symptom is every deploy, restart and
  # scale-down stalling for Docker's full 10s grace period.
  start=$(date +%s)
  docker stop -t 15 "${CONTAINER}" >/dev/null 2>&1
  elapsed=$(( $(date +%s) - start ))
  if [ "${elapsed}" -le 4 ]; then
    ok "exits on SIGTERM (${elapsed}s)"
  else
    bad "exits on SIGTERM" "took ${elapsed}s — SIGTERM ignored, killed by timeout" "${example}"
  fi
  cleanup

  # ---------------------------------------------------------- 9. honours PORT
  # Temps injects $PORT. An app hard-coded to its default silently never
  # receives traffic.
  docker run -d --name "${CONTAINER}" -e PORT="${CUSTOM_PORT}" \
    -p "${PORT_B}:${CUSTOM_PORT}" "${tag}" >/dev/null 2>&1
  if wait_for_http "${PORT_B}" 30; then
    ok "honours \$PORT (${CUSTOM_PORT})"
  else
    bad "honours \$PORT" "no response on ${CUSTOM_PORT}" "${example}"
    docker logs "${CONTAINER}" 2>&1 | tail -6 | sed 's/^/      /'
  fi
  cleanup

  # ------------------------------------- 10. source edit does not reinstall
  # The whole point of splitting install from build. If a one-character source
  # change busts the dependency layer, every deploy pays a full install.
  src=$(find "examples/${example}" -type f \
        \( -name '*.js' -o -name '*.ts' -o -name '*.go' -o -name '*.rs' -o -name '*.py' \
           -o -name '*.rb' -o -name '*.php' -o -name '*.ex' -o -name '*.java' \
           -o -name '*.cs' -o -name '*.cpp' -o -name '*.hs' -o -name '*.html' \) \
        ! -path '*/node_modules/*' 2>/dev/null | head -1)

  if [ -z "${src}" ] || ! "${AUTOPACK}" dockerfile "examples/${example}" -o "${WORK}/Dockerfile" 2>/dev/null; then
    skip "source edit reuses the install layer" "no source file found"
  elif ! grep -q '^# ---- install ----' "${WORK}/Dockerfile"; then
    skip "source edit reuses the install layer" "provider has no separate install step"
  else
    # A blank line changes the content hash (which is what Docker keys on)
    # while staying valid in every language.
    printf '\n' >>"${src}"
    docker buildx build --file "${WORK}/Dockerfile" --progress=plain \
      -t "${tag}-recache" "examples/${example}" >"${WORK}/rebuild.log" 2>&1
    build_status=$?
    # Undo the edit whatever happened, so the tree is never left dirty.
    if [ "$(uname)" = "Darwin" ]; then
      sed -i '' -e '$ d' "${src}"
    else
      sed -i -e '$ d' "${src}"
    fi

    if [ "${build_status}" -ne 0 ]; then
      bad "source edit reuses the install layer" "rebuild failed" "${example}"
    else
      install_stage=$(awk '/autopack-install/{f=1} f' "${WORK}/rebuild.log" | head -40)
      if echo "${install_stage}" | grep -q "CACHED"; then
        ok "source edit reuses the install layer"
      else
        bad "source edit reuses the install layer" "install re-ran on a source-only change" "${example}"
      fi
    fi
    docker rmi -f "${tag}-recache" >/dev/null 2>&1
  fi
done

printf '\n========================================\n'
printf 'checks passed: %d\n' "${pass}"
printf 'checks failed: %d\n' "${fail}"

if [ "${#REPORT[@]}" -gt 0 ]; then
  printf '\nimage facts:\n'
  printf '  %s\n' "${REPORT[@]}"
fi

if [ "${fail}" -gt 0 ]; then
  printf '\nfailures:\n'
  printf '  - %s\n' "${FAILURES[@]}"
  exit 1
fi
echo "all conformance checks passed"
