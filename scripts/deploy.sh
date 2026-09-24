#!/usr/bin/env bash
#
# MicroCrop Soroban deployment — deploy, wire and configure all four contracts.
#
# WHY A SCRIPT AND NOT A CHECKLIST. The four contracts are not independent: PayoutReceiver
# takes Treasury and PolicyManager addresses in its constructor, and six cross-contract ROLE
# grants have to be made after deployment or the money path silently does not work. A missed
# grant does not fail at deploy time — it fails later, on the first payout, on chain, with
# real money. The sequence below is lifted from crates/integration-tests, which exercises the
# identical wiring against all four real contracts, so it is verified rather than remembered.
#
# IDEMPOTENT AND RESUMABLE. Every deployed address is written to the state file as soon as it
# is known. Re-running skips what is already done. A deploy that dies halfway (a funding
# problem, a dropped connection) is resumed by running the same command again — never by
# starting over, which would orphan the contracts already paid for.
#
# Usage:
#   ./scripts/deploy.sh --network testnet
#   ./scripts/deploy.sh --network testnet --dry-run
#   ./scripts/deploy.sh --network mainnet          # requires typing the confirmation phrase
#
set -euo pipefail

NETWORK="testnet"
DRY_RUN=0
STATE_DIR="deployments"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="${2:?--network needs a value}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --state-dir) STATE_DIR="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

STATE="${STATE_DIR}/${NETWORK}.json"
mkdir -p "$STATE_DIR"
[[ -f "$STATE" ]] || echo '{}' > "$STATE"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
say()  { printf '\033[1;36m[deploy]\033[0m %s\n' "$*"; }
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
: "${USDC_SAC:?USDC_SAC is required — the USDC Stellar Asset Contract address for this network}"
: "${BACKEND_WALLET:?BACKEND_WALLET is required — G... address that receives platform fees}"
: "${BACKEND_ADDR:?BACKEND_ADDR is required — G... address the backend signs policy/premium calls with}"
: "${RELAYER_ADDR:?RELAYER_ADDR is required — G... address allowed to submit determinations}"

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

WASM_DIR="target/wasm32v1-none/release"
for w in microcrop_policy_nft microcrop_policy_manager microcrop_treasury microcrop_payout_receiver; do
  [[ -f "$WASM_DIR/$w.wasm" ]] || die "missing $WASM_DIR/$w.wasm — run: cargo build --release --target wasm32v1-none"
done

if [[ "$NETWORK" == "mainnet" && "$DRY_RUN" == "0" ]]; then
  warn "MAINNET deploy. These contracts have had NO Soroban security audit."
  read -r -p "Type 'deploy to mainnet' to continue: " confirm
  [[ "$confirm" == "deploy to mainnet" ]] || die "aborted"
fi

say "network=$NETWORK  state=$STATE  dry-run=$DRY_RUN"

# ---------------------------------------------------------------------------
# 1. Deploy — order matters: PayoutReceiver's constructor consumes two addresses
# ---------------------------------------------------------------------------
deploy() {                      # deploy <state-key> <wasm-name> [--ctor-args...]
  local key="$1" wasm="$2"; shift 2
  local existing; existing="$(state_get "$key")"
  if [[ -n "$existing" ]]; then say "$key already deployed: $existing (skipping)"; echo "$existing"; return; fi

  if [[ "$DRY_RUN" == "1" ]]; then
    printf '  \033[2mDRY-RUN:\033[0m deploy %s %s\n' "$wasm" "$*" >&2
    echo "C_DRYRUN_${key}"; return
  fi

  local addr
  addr="$(stellar contract deploy \
      --wasm "$WASM_DIR/$wasm.wasm" \
      --source "$ADMIN_KEY" --network "$NETWORK" \
      -- "$@" | tail -1)"
  [[ "$addr" =~ ^C[A-Z0-9]{55}$ ]] || die "$key deploy returned an unexpected address: $addr"
  state_set "$key" "$addr"
  say "$key deployed: $addr"
  echo "$addr"
}

ADMIN_ADDR="$(stellar keys address "$ADMIN_KEY" 2>/dev/null || echo "G_DRYRUN_ADMIN")"
say "admin address: $ADMIN_ADDR"

NFT="$(deploy policy_nft microcrop_policy_nft \
        --admin "$ADMIN_ADDR" --name "MicroCrop Policy" --symbol "MCP")"
PM="$(deploy policy_manager microcrop_policy_manager \
        --admin "$ADMIN_ADDR")"
TREASURY="$(deploy treasury microcrop_treasury \
        --usdc "$USDC_SAC" --backend_wallet "$BACKEND_WALLET" --admin "$ADMIN_ADDR")"
PR="$(deploy payout_receiver microcrop_payout_receiver \
        --treasury "$TREASURY" --policy_manager "$PM" --admin "$ADMIN_ADDR")"

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
step wire_treasury_pm   "$TREASURY" set_policy_manager  --policy_manager "$PM"
step wire_treasury_pr   "$TREASURY" set_payout_receiver --payout_receiver "$PR"

say "--- granting cross-contract roles ---"
# Without these the deploy LOOKS fine and the money path is dead: the backend cannot create a
# policy, PolicyManager cannot mint a certificate, and PayoutReceiver cannot draw a payout.
step role_pm_backend    "$PM"       grant_role --role Backend --who "$BACKEND_ADDR"
step role_tr_backend    "$TREASURY" grant_role --role Backend --who "$BACKEND_ADDR"
step role_nft_minter    "$NFT"      grant_role --role Minter  --who "$PM"
step role_pm_oracle     "$PM"       grant_role --role Oracle  --who "$PR"
step role_tr_payout     "$TREASURY" grant_role --role Payout  --who "$PR"
step role_pr_relayer    "$PR"       grant_role --role Relayer --who "$RELAYER_ADDR"

say "--- configuring the payout authority ---"
step cfg_signer  "$PR" set_authorized_signer --pubkey "${AUTHORIZED_SIGNER_PUBKEY#0x}"
step cfg_domain  "$PR" set_network_domain    --domain "$NETWORK_DOMAIN"

state_set network_domain "$NETWORK_DOMAIN"
state_set network "$NETWORK"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
cat <<EOF

========== DEPLOYED ($NETWORK) ==========
PolicyNft        $NFT
PolicyManager    $PM
Treasury         $TREASURY
PayoutReceiver   $PR

NETWORK_DOMAIN   $NETWORK_DOMAIN
  ^ the off-chain signer MUST use this exact value, or no determination will ever verify.

State: $STATE
EOF

if [[ "$DRY_RUN" == "1" ]]; then
  say "dry run only — nothing was deployed"
else
  say "next: run ./scripts/verify-deployment.sh --network $NETWORK"
fi
