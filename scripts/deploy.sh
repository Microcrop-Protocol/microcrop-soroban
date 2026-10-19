#!/usr/bin/env bash
#
# MicroCrop Soroban deployment — deploy, wire and configure all four contracts.
#
# The four contracts (Option B, 2026-10-19): PolicyNft, PolicyManager, SettlementAdapter,
# PayoutReceiver. There is no Treasury and no USDC token anywhere: a verified determination
# becomes an obligation in the SettlementAdapter, settled off-chain.
#
# WHY A SCRIPT AND NOT A CHECKLIST. The four contracts are not independent: PayoutReceiver
# takes the SettlementAdapter and PolicyManager addresses in its constructor, the adapter has
# to be bound back to the PayoutReceiver, and five cross-contract ROLE grants have to be made
# after deployment or the claim path silently does not work. A missed step does not fail at
# deploy time — it fails later, on the first claim, on chain. The sequence below is lifted
# from crates/integration-tests, which exercises the identical wiring against all four real
# contracts, so it is verified rather than remembered.
#
# IDEMPOTENT AND RESUMABLE. Every deployed address is written to the state file as soon as it
# is known. Re-running skips what is already done. A deploy that dies halfway (a funding
# problem, a dropped connection) is resumed by running the same command again — never by
# starting over, which would orphan the contracts already paid for.
#
# WHICH WASM. Two sources:
#   --release vX.Y.Z  the SEP-55 verified build: the four assets .github/workflows/release.yml
#                     attached to the per-contract releases of git tag vX.Y.Z. Each is
#                     downloaded to <state-dir>/releases/vX.Y.Z/, its GitHub build attestation
#                     is verified (signer workflow, pinned digest, source ref = the tag, this
#                     repo) and its contractmetav0 source_repo checked, and THOSE files are
#                     deployed, byte for byte. MAINNET ACCEPTS NOTHING ELSE.
#   (no --release)    the local build in target/wasm32v1-none/release, optimized here.
#                     Testnet only: nothing ties a local build to a commit.
#
# Usage:
#   ./scripts/deploy.sh --network testnet
#   ./scripts/deploy.sh --network testnet --dry-run
#   ./scripts/deploy.sh --network testnet --release v0.2.0
#   ./scripts/deploy.sh --network mainnet --release v0.2.0   # + the confirmation phrase
#   ./scripts/deploy.sh --network testnet --state-dir <dir>   # a fresh deployment elsewhere
#   ./scripts/deploy.sh ... --release v0.2.0 --release-dir <dir>
#                     use assets already in <dir> (one <crate>_v*.wasm each) instead of
#                     downloading; they are still verified. Test hook: with --dry-run only,
#                     DRY_RUN_SKIP_ATTESTATION=1 skips verification (fake assets).
#
# Environment: ADMIN_KEY, BACKEND_ADDR, RELAYER_ADDR, AUTHORIZED_SIGNER_PUBKEY (required);
# FUNDER_ADDR (optional, default BACKEND_ADDR: who may mark obligations funded/settled);
# NETWORK_DOMAIN (optional, default sha256(passphrase)).
#
set -euo pipefail

NETWORK="testnet"
DRY_RUN=0
STATE_DIR="deployments"
RELEASE=""
RELEASE_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="${2:?--network needs a value}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --state-dir) STATE_DIR="${2:?}"; shift 2 ;;
    --release) RELEASE="${2:?--release needs a tag, e.g. v0.2.0}"; shift 2 ;;
    --release-dir) RELEASE_DIR="${2:?--release-dir needs a directory}"; shift 2 ;;
    -h|--help) sed -n '2,46p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

STATE="${STATE_DIR}/${NETWORK}.json"
mkdir -p "$STATE_DIR"
[[ -f "$STATE" ]] || echo '{}' > "$STATE"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
say()  { printf '\033[1;36m[deploy]\033[0m %s\n' "$*" >&2; }  # stderr: deploy() returns the address on stdout
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[fatal]\033[0m %s\n' "$*" >&2; exit 1; }

# Read/write the state file. Addresses are recorded the moment they exist, so a crash between
# two steps never loses a deployed contract.
state_get() { jq -r --arg k "$1" '.[$k] // empty' "$STATE"; }
state_set() {
  local tmp; tmp="$(mktemp)"
  jq --arg k "$1" --arg v "$2" '.[$k] = $v' "$STATE" > "$tmp" && mv "$tmp" "$STATE"
}

run() {
  if [[ "$DRY_RUN" == "1" ]]; then printf '  \033[2mDRY-RUN:\033[0m %s\n' "$*"; return 0; fi
  "$@"
}

# ---------------------------------------------------------------------------
# Preflight — fail before spending anything, not halfway through
# ---------------------------------------------------------------------------
command -v stellar >/dev/null || die "stellar CLI not found. Install: cargo install --locked stellar-cli"
command -v jq      >/dev/null || die "jq not found (brew install jq)"

: "${ADMIN_KEY:?ADMIN_KEY is required — the stellar CLI identity that will hold Admin+Upgrader on all four contracts}"
: "${BACKEND_ADDR:?BACKEND_ADDR is required — G... address the backend signs policy calls with}"
: "${RELAYER_ADDR:?RELAYER_ADDR is required — G... address allowed to submit determinations}"
# The Funder (and only the Funder: never the org that owes it) may mark ANY org's obligation
# funded / settled in the SettlementAdapter. These are
# attestations by the paying agent (the backend's mobile-money off-ramp), not proof of payment.
FUNDER_ADDR="${FUNDER_ADDR:-$BACKEND_ADDR}"

# The 65-byte uncompressed SEC-1 public key of the Lit PKP, hex, WITH the 0x04 prefix.
# NOT the 0x… Ethereum address: the contract recovers a pubkey from the signature and compares
# it to this, so an address here silently fails every signature check.
: "${AUTHORIZED_SIGNER_PUBKEY:?AUTHORIZED_SIGNER_PUBKEY is required — 130 hex chars (65 bytes, 0x04-prefixed SEC-1)}"

[[ "${AUTHORIZED_SIGNER_PUBKEY#0x}" =~ ^04[0-9a-fA-F]{128}$ ]] \
  || die "AUTHORIZED_SIGNER_PUBKEY must be 65 bytes of SEC-1 hex starting 04 (130 chars). Got ${#AUTHORIZED_SIGNER_PUBKEY} chars. An Ethereum 0x-address will NOT work."

case "$NETWORK" in
  testnet) PASSPHRASE="Test SDF Network ; September 2015" ;;
  mainnet|public) PASSPHRASE="Public Global Stellar Network ; September 2015"; NETWORK="mainnet" ;;
  *) die "unknown network: $NETWORK (expected testnet|mainnet)" ;;
esac

# NETWORK_DOMAIN replaces EVM `block.chainid` as the replay-domain separator. Default:
# sha256(network passphrase). That mirrors what chainid did — it binds a signed determination
# to ONE network, so a testnet signature can never be replayed on mainnet — and it is
# derivable by both sides from a public constant, so the off-chain signer and the contract can
# agree without shipping a magic number. Override only if the signer already committed to a
# different value; BOTH SIDES MUST MATCH BYTE FOR BYTE or no signature will ever verify.
if [[ -z "${NETWORK_DOMAIN:-}" ]]; then
  NETWORK_DOMAIN="$(printf '%s' "$PASSPHRASE" | shasum -a 256 | cut -d' ' -f1)"
  say "NETWORK_DOMAIN derived = sha256(passphrase) = $NETWORK_DOMAIN"
else
  [[ "${NETWORK_DOMAIN#0x}" =~ ^[0-9a-fA-F]{64}$ ]] || die "NETWORK_DOMAIN must be 32 bytes of hex (64 chars)"
  NETWORK_DOMAIN="${NETWORK_DOMAIN#0x}"
  say "NETWORK_DOMAIN (explicit) = $NETWORK_DOMAIN"
fi

# ---------------------------------------------------------------------------
# The WASM to deploy: the attested release assets, or (testnet only) the local build
# ---------------------------------------------------------------------------
[[ -z "$RELEASE_DIR" || -n "$RELEASE" ]] || die "--release-dir needs --release <tag> (the tag the assets must be attested to)"
if [[ "${DRY_RUN_SKIP_ATTESTATION:-0}" == "1" && "$DRY_RUN" != "1" ]]; then
  die "DRY_RUN_SKIP_ATTESTATION is a --dry-run test hook; refusing to deploy unverified WASM"
fi
if [[ "$NETWORK" == "mainnet" && -z "$RELEASE" ]]; then
  die "mainnet deploys only the attested release WASM: pass --release vX.Y.Z (see DEPLOYMENT_RUNBOOK, Cutting a release). A local build is refused on mainnet."
fi

# A deployment is one build. Never resume a state file with a different one.
recorded="$(state_get release)"
started="$(jq -r '[.policy_nft, .policy_manager, .settlement_adapter, .payout_receiver] | map(select(. != null)) | length' "$STATE")"
if [[ -n "$RELEASE" ]]; then
  [[ -z "$recorded" || "$recorded" == "$RELEASE" ]] \
    || die "state file records release $recorded; resuming with $RELEASE would mix builds. Use --state-dir for a new deployment."
  [[ -n "$recorded" || "$started" == "0" ]] \
    || die "state file holds contracts deployed from a LOCAL build; resuming with --release would mix builds. Use --state-dir for a new deployment."
else
  [[ -z "$recorded" ]] \
    || die "state file records release $recorded; resume it with --release $recorded, not a local build"
fi

# deploy() looks the file up by state key: WASM_<key>.
if [[ -n "$RELEASE" ]]; then
  # shellcheck source=scripts/lib/release.sh
  source "$(dirname "$0")/lib/release.sh"
  release_check_tag "$RELEASE"
  if [[ -n "$RELEASE_DIR" ]]; then
    [[ -d "$RELEASE_DIR" ]] || die "--release-dir $RELEASE_DIR does not exist"
  else
    command -v gh >/dev/null || die "gh CLI not found (brew install gh): --release downloads and verifies with it"
    RELEASE_DIR="${STATE_DIR}/releases/${RELEASE}"
    release_download "$RELEASE" "$RELEASE_DIR"
  fi
  [[ "${DRY_RUN_SKIP_ATTESTATION:-0}" == "1" ]] && RELEASE_SKIP_VERIFY=1
  commits=()
  for pair in "${RELEASE_CONTRACTS[@]}"; do
    key="${pair%%:*}"; crate="${pair#*:}"
    file="$(release_asset "$RELEASE_DIR" "$crate")"
    commit="$(release_verify_asset "$RELEASE" "$file")"
    commits+=("$commit")
    printf -v "WASM_${key}" '%s' "$file"
    say "$crate: $(basename "$file")  sha256 $(sha256_of "$file")  built from $commit"
  done
  # All four from one commit, and the commit the tag points at now. Fatal otherwise.
  REL_COMMIT="$(release_check_commits "$RELEASE" "${commits[@]}")"
  say "release $RELEASE: all four assets built from $REL_COMMIT"
else
  WASM_DIR="target/wasm32v1-none/release"
  for pair in policy_nft:microcrop_policy_nft policy_manager:microcrop_policy_manager \
              settlement_adapter:microcrop_settlement_adapter payout_receiver:microcrop_payout_receiver; do
    key="${pair%%:*}"; w="${pair#*:}"
    [[ -f "$WASM_DIR/$w.wasm" ]] || die "missing $WASM_DIR/$w.wasm — run: cargo build --release --target wasm32v1-none"
    # Deploy the OPTIMIZED wasm: it is ~16-22% smaller, and code rent is paid per byte for the
    # contract's whole life. The rent figures in the docs assume these files.
    if [[ ! -f "$WASM_DIR/$w.optimized.wasm" || "$WASM_DIR/$w.wasm" -nt "$WASM_DIR/$w.optimized.wasm" ]]; then
      say "optimizing $w.wasm"
      run stellar contract optimize --wasm "$WASM_DIR/$w.wasm" >&2
    fi
    printf -v "WASM_${key}" '%s' "$WASM_DIR/$w.optimized.wasm"
  done
  warn "deploying a LOCAL build: no attestation ties it to a commit (testnet only)"
fi

if [[ "$NETWORK" == "mainnet" && "$DRY_RUN" == "0" ]]; then
  warn "MAINNET deploy of release $RELEASE. These contracts have had NO Soroban security audit."
  read -r -p "Type 'deploy to mainnet' to continue: " confirm
  [[ "$confirm" == "deploy to mainnet" ]] || die "aborted"
fi

say "network=$NETWORK  state=$STATE  dry-run=$DRY_RUN  wasm=${RELEASE:-local build}"

# A state file from a Treasury-era deployment cannot be resumed into Option B: its
# PayoutReceiver was constructed with a Treasury and has no adapter. Deploy fresh elsewhere.
if [[ -n "$(state_get treasury)" ]]; then
  die "state file belongs to a Treasury deployment; use --state-dir"
fi

# ---------------------------------------------------------------------------
# 1. Deploy — order matters: PayoutReceiver's constructor consumes two addresses
# ---------------------------------------------------------------------------
deploy() {                      # deploy <state-key> [--ctor-args...]
  local key="$1"; shift
  local wasm_var="WASM_${key}" wasm
  wasm="${!wasm_var:?no WASM resolved for $key}"
  local existing; existing="$(state_get "$key")"
  if [[ -n "$existing" ]]; then say "$key already deployed: $existing (skipping)"; echo "$existing"; return; fi

  if [[ "$DRY_RUN" == "1" ]]; then
    printf '  \033[2mDRY-RUN:\033[0m deploy %s %s\n' "$wasm" "$*" >&2
    echo "C_DRYRUN_${key}"; return
  fi

  local addr
  addr="$(stellar contract deploy \
      --wasm "$wasm" \
      --source "$ADMIN_KEY" --network "$NETWORK" \
      -- "$@" | tail -1)"
  [[ "$addr" =~ ^C[A-Z0-9]{55}$ ]] || die "$key deploy returned an unexpected address: $addr"
  state_set "$key" "$addr"
  state_set "wasm_sha256_${key}" "$(shasum -a 256 "$wasm" | cut -d' ' -f1)"
  say "$key deployed: $addr"
  echo "$addr"
}

ADMIN_ADDR="$(stellar keys address "$ADMIN_KEY" 2>/dev/null || echo "G_DRYRUN_ADMIN")"
say "admin address: $ADMIN_ADDR"

# Recorded before the first deploy, so a resumed run cannot switch builds midway.
[[ -n "$RELEASE" && "$DRY_RUN" == "0" ]] && state_set release "$RELEASE"

NFT="$(deploy policy_nft \
        --admin "$ADMIN_ADDR" --name "MicroCrop Policy" --symbol "MCP")"
PM="$(deploy policy_manager \
        --admin "$ADMIN_ADDR")"
ADAPTER="$(deploy settlement_adapter \
        --admin "$ADMIN_ADDR")"
PR="$(deploy payout_receiver \
        --settlement_adapter "$ADAPTER" --policy_manager "$PM" --admin "$ADMIN_ADDR")"

# ---------------------------------------------------------------------------
# 2. Wire + 3. Roles + 4. Configure
#
# Each step is recorded in the state file so a resumed run does not repeat it. Repeating is
# harmless for setters but costs a transaction, and on mainnet that is real money.
# ---------------------------------------------------------------------------
step() {                        # step <state-key> <contract-addr> <fn> [--args...]
  local key="$1" contract="$2" fn="$3"; shift 3
  if [[ -n "$(state_get "$key")" ]]; then say "$key already done (skipping)"; return; fi
  run stellar contract invoke --id "$contract" --source "$ADMIN_KEY" --network "$NETWORK" \
      -- "$fn" --caller "$ADMIN_ADDR" "$@"
  [[ "$DRY_RUN" == "1" ]] || state_set "$key" "done"
  say "$key ✓"
}

say "--- wiring cross-contract references ---"
step wire_pm_nft        "$PM"       set_policy_nft      --policy_nft "$NFT"
step wire_nft_pm        "$NFT"      set_policy_manager  --policy_manager "$PM"
# The handshake: until the adapter names the PayoutReceiver, every determination reverts
# (NotPayoutReceiver, 500) — fail closed, but nothing settles.
step wire_adapter_pr    "$ADAPTER"  set_payout_receiver --payout_receiver "$PR"

say "--- granting cross-contract roles ---"
# Without these the deploy LOOKS fine and the claim path is dead: the backend cannot create a
# policy, PolicyManager cannot mint a certificate, PayoutReceiver cannot mark a policy claimed,
# and no obligation can ever be marked funded or settled.
# Role is a #[repr(u32)] contracttype enum, so the CLI takes its NUMBER, not its name:
# Admin=0 Backend=1 Oracle=2 (3 retired) Relayer=4 Upgrader=5 Minter=6 Funder=7
# (crates/shared/src/types.rs).
step role_pm_backend    "$PM"       grant_role --role 1 --who "$BACKEND_ADDR"
step role_nft_minter    "$NFT"      grant_role --role 6 --who "$PM"
step role_pm_oracle     "$PM"       grant_role --role 2 --who "$PR"
step role_pr_relayer    "$PR"       grant_role --role 4 --who "$RELAYER_ADDR"
step role_ad_funder     "$ADAPTER"  grant_role --role 7 --who "$FUNDER_ADDR"

say "--- configuring the payout authority ---"
step cfg_signer  "$PR" set_authorized_signer --pubkey "${AUTHORIZED_SIGNER_PUBKEY#0x}"
step cfg_domain  "$PR" set_network_domain    --domain "$NETWORK_DOMAIN"
# MethodologyKind is a #[repr(u32)] contracttype enum too: CropDualIndex10=0 (CROP_DAMAGE,
# retired: leave disabled), CropDroughtRdi10=1 (CROP_DROUGHT). Absent means disabled, so
# nothing settles until this step has run.
step cfg_drought "$PR" set_methodology_enabled --kind 1 --enabled true

# Rent cap: ordinary calls extend instance + code only below 3 days, to 10 days, and admin
# calls extend nothing, so a fresh upload sits at the network minimum (7 days on testnet).
# The permissionless keeper (PayoutReceiver.extend_contract_ttl) extends all four contracts —
# itself, the SettlementAdapter, PolicyManager and PolicyNft — to 30 days; the backend runs it
# daily (a no-op above 23 days). Not recorded as a step: it is meant to be repeated.
# --send=yes is required: a TTL extension writes no ledger entry, emits no event and needs no
# auth, so the CLI's default classifies the simulation as read-only and never submits it
# ("Simulation identified as read-only. Send by rerunning with `--send=yes`.").
say "--- keeper: extend all four contracts' instance + code to 30 days ---"
run stellar contract invoke --id "$PR" --source "$ADMIN_KEY" --network "$NETWORK" --send=yes \
    -- extend_contract_ttl

state_set network_domain "$NETWORK_DOMAIN"
state_set network "$NETWORK"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
cat <<EOF

========== DEPLOYED ($NETWORK) ==========
PolicyNft        $NFT
PolicyManager    $PM
SettlementAdapter $ADAPTER
PayoutReceiver   $PR

Funder (adapter) $FUNDER_ADDR

NETWORK_DOMAIN   $NETWORK_DOMAIN
  ^ the off-chain signer MUST use this exact value, or no determination will ever verify.

State: $STATE
EOF

if [[ "$DRY_RUN" == "1" ]]; then
  say "dry run only — nothing was deployed"
else
  say "next: run ./scripts/verify-deployment.sh --network $NETWORK${RELEASE:+ --release $RELEASE}"
fi
