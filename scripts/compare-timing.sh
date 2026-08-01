#!/usr/bin/env bash
#
# Cold-cache build timing for nixpacks, railpack and autopack.
#
# Timings taken from the outcome sweep are worthless: whichever builder ran
# last inherits warm layers. This clears each builder's cache before every
# single build so the numbers are comparable.
#
# Base images are deliberately NOT pruned. Pulling Debian or a Nix base is a
# property of the machine's registry cache, not of the builder, and including
# it would just measure network variance.
#
set -uo pipefail

CORPUS="${1:?usage: compare-timing.sh <corpus-dir>}"
cd "$(dirname "$0")/.." || exit 1
AUTOPACK="${AUTOPACK:-./target/debug/autopack}"
export BUILDKIT_HOST="docker-container://buildkit"

# Only apps all three builders can actually build, so the comparison is of
# speed rather than of failure.
APPS=("nodejs/express" "python/fastapi" "vite/react" "php/vanilla")

reset_docker_cache() { docker builder prune -af >/dev/null 2>&1; }

reset_buildkit() {
  docker rm -f buildkit >/dev/null 2>&1
  docker run --rm --privileged -d --name buildkit moby/buildkit:latest >/dev/null 2>&1
  sleep 4
}

printf 'app\tbuilder\tcold_secs\n'

for app in "${APPS[@]}"; do
  dir="${CORPUS}/${app}"
  [ -d "${dir}" ] || continue
  slug=$(echo "${app}" | tr '/' '-')

  for builder in nixpacks railpack autopack; do
    case "${builder}" in
      railpack) reset_buildkit ;;
      *) reset_docker_cache ;;
    esac

    start=$(date +%s)
    case "${builder}" in
      nixpacks) nixpacks build "${dir}" --name "t-${slug}" >/dev/null 2>&1 ;;
      railpack) railpack build "${dir}" --name "t-${slug}" >/dev/null 2>&1 ;;
      autopack) "${AUTOPACK}" build "${dir}" -t "t-${slug}:t" >/dev/null 2>&1 ;;
    esac
    status=$?
    secs=$(( $(date +%s) - start ))

    if [ "${status}" -ne 0 ]; then
      printf '%s\t%s\tfailed\n' "${app}" "${builder}"
    else
      printf '%s\t%s\t%s\n' "${app}" "${builder}" "${secs}"
    fi
  done
done
