#!/usr/bin/env bash
#
# Verify a SEP-55 release before using its WASM outside deploy.sh (an upgrade, an audit, a
# reproduction). Downloads the four per-contract assets of <tag> (or uses --dir), checks each
# one's build attestation (pinned signer workflow and digest, refs/tags/<tag>, GitHub-hosted
# runner) and source_repo metadata, then checks that all four were built from one commit and
# that the tag still points at it. Prints each file's sha256: that is the hash `upgrade` takes.
#
#   ./scripts/verify-release.sh v0.2.0
#   ./scripts/verify-release.sh v0.2.0 --dir ./releases/v0.2.0
#
# Exits non-zero on any failure. Never deploys, uploads or signs anything.
set -euo pipefail

usage() { sed -n '3,12p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

TAG="" DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dir) DIR="${2:?--dir needs a directory}"; shift 2 ;;
    -h|--help) usage 0 ;;
    -*) echo "unknown option $1" >&2; usage 2 ;;
    *) [[ -z "$TAG" ]] || usage 2; TAG="$1"; shift ;;
  esac
done
[[ -n "$TAG" ]] || usage 2

# shellcheck source=scripts/lib/release.sh
source "$(dirname "$0")/lib/release.sh"
for tool in gh jq stellar shasum; do
  command -v "$tool" >/dev/null || _rel_die "$tool not found"
done
release_check_tag "$TAG"
DIR="${DIR:-releases/$TAG}"
release_download "$TAG" "$DIR"

commits=()
for pair in "${RELEASE_CONTRACTS[@]}"; do
  crate="${pair#*:}"
  file="$(release_asset "$DIR" "$crate")"
  commit="$(release_verify_asset "$TAG" "$file")"
  commits+=("$commit")
  printf '%s  sha256 %s  built from %s\n' "$file" "$(sha256_of "$file")" "$commit"
done
one="$(release_check_commits "$TAG" "${commits[@]}")"
_rel_say "OK: release $TAG, all four assets attested and built from $one"
