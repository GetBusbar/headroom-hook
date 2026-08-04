#!/usr/bin/env bash
# next-version.sh — compute THIS repo's next release tag for release-on-upstream.yml.
#
# Single source of truth for the version math, exercised in CI by release-selftest.yml so the
# release automation can't silently rot (guard #135.8). Prints "v<MAJOR>.<MINOR>.<PATCH>" to stdout.
#
# Inputs (env, all optional):
#   INPUT_VERSION    explicit version to cut (leading "v" tolerated) -> used verbatim.
#   INITIAL_VERSION  first-release default when the repo has NO prior v* tag (default "1.0.0").
#
# Behaviour:
#   * explicit INPUT_VERSION            -> v<INPUT_VERSION>
#   * no prior v* tag (brand-new repo)  -> v<INITIAL_VERSION>   (NEVER errors — first release)
#   * otherwise                         -> patch-bump the highest existing v* tag
# Fully `set -u` safe: every variable is initialised before use, so no "unbound variable".
set -euo pipefail

input="${INPUT_VERSION:-}"
initial="${INITIAL_VERSION:-1.0.0}"

if [ -n "$input" ]; then
  printf 'v%s\n' "${input#v}"
  exit 0
fi

latest="$(git tag --list 'v*' | sort -V | tail -1 || true)"
if [ -z "${latest:-}" ]; then
  # FIRST RELEASE: no prior v* tag. Default to the declared initial version instead of
  # erroring, so a brand-new plugin is cuttable by the release train.
  printf 'v%s\n' "${initial#v}"
  exit 0
fi

base="${latest#v}"
major="${base%%.*}";  rest="${base#*.}"
minor="${rest%%.*}";  patch="${rest#*.}"
patch="${patch%%[-+]*}"          # drop any -rc / +build suffix on the patch component
# Coerce every component to a non-negative integer so arithmetic under set -u never explodes.
case "$major" in ''|*[!0-9]*) major=0 ;; esac
case "$minor" in ''|*[!0-9]*) minor=0 ;; esac
case "$patch" in ''|*[!0-9]*) patch=0 ;; esac
printf 'v%s.%s.%s\n' "$major" "$minor" "$((patch + 1))"
