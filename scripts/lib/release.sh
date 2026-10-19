# shellcheck shell=bash
#
# Fetch and verify the SEP-55 release assets that .github/workflows/release.yml publishes.
# Sourced by scripts/deploy.sh and scripts/verify-deployment.sh; defines functions only.
#
# One git tag vX.Y.Z produces one GitHub release PER CONTRACT, named by the reusable
# stellar-expert/soroban-build-workflow as
#     vX.Y.Z_<crate>[_pkg<crate-version>]_cli<cli-version>
# each carrying one asset, <crate>_v<crate-version>.wasm (already optimized), and one build
# attestation over that file.
#
# What release_verify_asset enforces (gh attestation verify):
#   --repo              the attestation belongs to this repository
#   --signer-workflow   it was signed by the reusable soroban-build-workflow release.yml. Needed:
#                       a plain `--repo` check expects a workflow IN this repo and rejects these
#                       ("verifying with issuer sigstore.dev").
#   --signer-digest     ...at a commit we pinned and reviewed (RELEASE_SIGNER_DIGESTS)
#   --source-ref        it was built from refs/tags/<the tag asked for>
#   --deny-self-hosted-runners
# plus: the WASM's contractmetav0 says source_repo=github:<this repository>.

RELEASE_REPO="Microcrop-Protocol/microcrop-soroban"
# Never taken from the environment: only deploy.sh sets it, and only under --dry-run.
RELEASE_SKIP_VERIFY=0
RELEASE_SIGNER_WORKFLOW="stellar-expert/soroban-build-workflow/.github/workflows/release.yml"
# Every soroban-build-workflow commit release.yml has ever been pinned to. Append on a bump;
# never remove one while a deployment built by it is live.
RELEASE_SIGNER_DIGESTS=(
  e43b241f114bcf02151c30d61088506c6ad3ae0b # v25.1.0, pinned 2026-10-19
)

# The four released contracts: <state key>:<crate>. The testnet fixture is never released.
RELEASE_CONTRACTS=(
  "policy_nft:microcrop-policy-nft"
  "policy_manager:microcrop-policy-manager"
  "settlement_adapter:microcrop-settlement-adapter"
  "payout_receiver:microcrop-payout-receiver"
)

_rel_die() { printf '\033[1;31m[fatal]\033[0m %s\n' "$*" >&2; exit 1; }
_rel_say() { printf '\033[1;36m[release]\033[0m %s\n' "$*" >&2; }

release_check_tag() {           # release_check_tag <tag>
  [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || _rel_die "release tag '$1' is not vMAJOR.MINOR.PATCH[-pre] (the git tag, not a per-contract release name)"
}

# The per-contract GitHub release for <tag> and <crate>. Exactly one must exist.
release_name_for() {            # release_name_for <tag> <crate> -> release tag name
  local tag="$1" crate="$2" re all names count
  re="^$(printf '%s' "$tag" | sed 's/[.]/[.]/g')_${crate}_(pkg[^_]+_)?cli[0-9][0-9.]*\$"
  all="$(gh release list --repo "$RELEASE_REPO" --limit 1000 --json tagName --jq '.[].tagName')" \
    || _rel_die "gh release list failed for $RELEASE_REPO (gh auth status?)"
  names="$(printf '%s\n' "$all" | grep -E "$re" || true)"
  count="$(printf '%s' "$names" | grep -c . || true)"
  [[ "$count" == "1" ]] \
    || _rel_die "expected exactly one release for $tag / $crate in $RELEASE_REPO, found $count: ${names:-none}. Did release.yml finish for that tag?"
  printf '%s\n' "$names"
}

# Download every contract's asset for <tag> into <dir> (skipped when the files are there).
release_download() {            # release_download <tag> <dir>
  local tag="$1" dir="$2" pair crate rel
  mkdir -p "$dir"
  for pair in "${RELEASE_CONTRACTS[@]}"; do
    crate="${pair#*:}"
    if compgen -G "$dir/${crate}_v*.wasm" >/dev/null; then
      _rel_say "$crate: using $(ls "$dir/${crate}"_v*.wasm)"
      continue
    fi
    rel="$(release_name_for "$tag" "$crate")"
    _rel_say "$crate: downloading from release $rel"
    gh release download "$rel" --repo "$RELEASE_REPO" --pattern "${crate}_v*.wasm" --dir "$dir" \
      || _rel_die "download of $rel failed"
  done
}

# The single asset for <crate> in <dir>.
release_asset() {               # release_asset <dir> <crate> -> path
  local dir="$1" crate="$2" files
  files=("$dir/${crate}"_v*.wasm)
  [[ -f "${files[0]}" ]] || _rel_die "no ${crate}_v*.wasm in $dir"
  [[ "${#files[@]}" == "1" ]] || _rel_die "more than one ${crate}_v*.wasm in $dir: ${files[*]}"
  printf '%s\n' "${files[0]}"
}

sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }

# Verify one asset. Prints the source commit the attestation names. Any failure is fatal.
release_verify_asset() {        # release_verify_asset <tag> <file> -> source commit sha
  local tag="$1" file="$2" digest out="" meta commit
  if [[ "${RELEASE_SKIP_VERIFY:-0}" == "1" ]]; then
    printf '\033[1;33m[warn]\033[0m %s\n' "$(basename "$file"): attestation NOT verified (dry-run test hook)" >&2
    printf 'unverified\n'; return 0
  fi
  command -v gh >/dev/null || _rel_die "gh CLI not found: it verifies the build attestations"
  for digest in "${RELEASE_SIGNER_DIGESTS[@]}"; do
    if out="$(gh attestation verify "$file" --repo "$RELEASE_REPO" \
                --signer-workflow "$RELEASE_SIGNER_WORKFLOW" --signer-digest "$digest" \
                --source-ref "refs/tags/$tag" --deny-self-hosted-runners \
                --format json 2>/dev/null)"; then
      break
    fi
    out=""
  done
  [[ -n "$out" ]] || _rel_die "$(basename "$file"): NO valid attestation from $RELEASE_SIGNER_WORKFLOW at a pinned digest, built from refs/tags/$tag in $RELEASE_REPO. Do not deploy it."
  commit="$(jq -r '.[0].verificationResult.signature.certificate.sourceRepositoryDigest // empty' <<<"$out")"

  meta="$(stellar contract info meta --wasm "$file" --output json 2>/dev/null \
          | jq -r '.[] | .sc_meta_v0 | select(.key == "source_repo") | .val')"
  [[ "$meta" == "github:$RELEASE_REPO" ]] \
    || _rel_die "$(basename "$file"): contractmetav0 source_repo is '${meta:-<none>}', expected github:$RELEASE_REPO"

  [[ "$commit" =~ ^[0-9a-f]{40}$ ]] \
    || _rel_die "$(basename "$file"): the attestation names no source commit (sourceRepositoryDigest '${commit:-<none>}'). Do not deploy it."
  printf '%s\n' "$commit"
}

# The commit refs/tags/<tag> points at now (annotated tags peeled).
release_tag_commit() {          # release_tag_commit <tag> -> sha
  local sha
  sha="$(gh api "repos/$RELEASE_REPO/commits/refs/tags/$1" --jq .sha 2>/dev/null)" || sha=""
  [[ "$sha" =~ ^[0-9a-f]{40}$ ]] || _rel_die "cannot resolve tag $1 in $RELEASE_REPO (deleted? gh auth status?)"
  printf '%s\n' "$sha"
}

# Every asset of one release must come from ONE commit, and it must be the commit the tag
# points at now. Each asset is attested against refs/tags/<tag>, but that does not stop a mix:
# if a release job fails after `gh release create` and someone deletes and re-pushes the tag on
# a different commit, the jobs whose releases survived keep the old build and only the deleted
# ones rebuild from the new commit. Same source ref, two commits. This catches it, and a tag
# that moved after the build.
release_check_commits() {       # release_check_commits <tag> <commit>... -> the one commit
  local tag="$1" first="$2" c tagsha
  shift
  for c in "$@"; do
    [[ "$c" == "$first" ]] \
      || _rel_die "release $tag mixes builds: assets were built from $first and $c. Cut a new tag; never move or re-push one."
  done
  if [[ "$first" == "unverified" ]]; then
    printf '%s\n' "$first"; return 0     # dry-run test hook: nothing was verified
  fi
  # `|| exit 1`: _rel_die inside $(...) ends only that subshell, and callers run this one in
  # $(...) too, where errexit is not inherited.
  tagsha="$(release_tag_commit "$tag")" || exit 1
  [[ "$tagsha" == "$first" ]] \
    || _rel_die "tag $tag now points at $tagsha but its assets were built from $first: the tag moved. Do not deploy it."
  printf '%s\n' "$first"
}
