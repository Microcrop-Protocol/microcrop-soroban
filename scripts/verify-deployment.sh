#!/usr/bin/env bash
#
# Verify a MicroCrop Soroban deployment (Option B: PolicyNft, PolicyManager, SettlementAdapter,
# PayoutReceiver): every contract live, every cross-reference wired (the PayoutReceiver and
# the SettlementAdapter bound to EACH OTHER), every role granted, the signer and network
# domain set, CROP_DROUGHT enabled and the retired CROP_DAMAGE methodology disabled, every
# instance and code entry kept alive by the keeper (at least 23 days of TTL left), and every
# deployed wasm hash equal to the local optimized build.
#
# WHY THIS EXISTS. None of the wiring fails at deploy time. A missed role grant or an unbound
# adapter produces a deployment that looks complete and is dead on the claim path — the
# backend cannot create a policy, PayoutReceiver cannot record an obligation, or nobody can
# mark one settled — and you discover it on the first real claim. This reads the deployed
# state back and says so before that happens.
#
# HOW IT CHECKS ROLES. Each contract exposes `has_role(role, who) -> bool` as a read-only
# view, so role checks are EXACT: the contract is asked directly rather than its storage being
# dumped and matched on addresses. An earlier version did the latter and could false-pass when
# an address appeared in storage for an unrelated reason.
#
# Usage: ./scripts/verify-deployment.sh --network testnet
#
set -euo pipefail

NETWORK="testnet"
STATE_DIR="deployments"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="${2:?}"; shift 2 ;;
    --state-dir) STATE_DIR="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,21p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

STATE="${STATE_DIR}/${NETWORK}.json"
[[ -f "$STATE" ]] || { echo "no deployment state at $STATE — run deploy.sh first" >&2; exit 1; }

command -v stellar >/dev/null || { echo "stellar CLI not found" >&2; exit 1; }
command -v jq      >/dev/null || { echo "jq not found" >&2; exit 1; }

get() { jq -r --arg k "$1" '.[$k] // empty' "$STATE"; }

NFT="$(get policy_nft)"; PM="$(get policy_manager)"
ADAPTER="$(get settlement_adapter)"; PR="$(get payout_receiver)"
DOMAIN="$(get network_domain)"
FUNDER_ADDR="${FUNDER_ADDR:-${BACKEND_ADDR:-}}"
if [[ -n "$(get treasury)" ]]; then
  echo "this state file belongs to a Treasury-era deployment; this script verifies Option B" >&2
  exit 1
fi

PASS=0; FAIL=0
ok()   { printf '  \033[1;32m✓\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

echo "Verifying $NETWORK deployment from $STATE"
echo

echo "1. Contracts deployed and reachable"
CONTRACTS=("PolicyNft:$NFT" "PolicyManager:$PM" "SettlementAdapter:$ADAPTER" "PayoutReceiver:$PR")
for pair in "${CONTRACTS[@]}"; do
  name="${pair%%:*}"; addr="${pair#*:}"
  if [[ -z "$addr" ]]; then bad "$name missing from state file"; continue; fi
  if [[ ! "$addr" =~ ^C[A-Z0-9]{55}$ ]]; then bad "$name has a malformed address: $addr"; continue; fi
  # `contract info interface` succeeds only against a real, live contract.
  if stellar contract info interface --id "$addr" --network "$NETWORK" >/dev/null 2>&1; then
    ok "$name live  $addr"
  else
    bad "$name NOT reachable on $NETWORK  $addr"
  fi
done
echo

# Dump each contract's storage once; the checks below grep these.
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
dump() {
  local addr="$1" out="$2"
  # Config refs, the signer and the domain live in INSTANCE storage, which a plain persistent
  # read does not return, and only --output json shows the instance storage map.
  # AAAAFA== is ScVal::LedgerKeyContractInstance.
  { stellar contract read --id "$addr" --network "$NETWORK" --durability persistent --key-xdr AAAAFA== --output json 2>/dev/null || true; } >  "$out"
  { stellar contract read --id "$addr" --network "$NETWORK" --durability persistent 2>/dev/null || true; } >> "$out"
  { stellar contract read --id "$addr" --network "$NETWORK" --durability temporary  2>/dev/null || true; } >> "$out"
}
[[ -n "$NFT"      ]] && dump "$NFT"      "$TMP/nft"
[[ -n "$PM"       ]] && dump "$PM"       "$TMP/pm"
[[ -n "$PR"       ]] && dump "$PR"       "$TMP/pr"

has() { [[ -f "$1" ]] && grep -qiF -- "$2" "$1"; }

# Read-only view: simulate, never submit. Prints the bare value (quotes stripped).
view() {                        # view <contract> <fn> [--args...]
  local contract="$1" fn="$2"; shift 2
  stellar contract invoke --id "$contract" --network "$NETWORK" --source "${ADMIN_KEY:-default}" \
      --send no -- "$fn" "$@" 2>/dev/null | tr -d '"' | tail -1
}

echo "2. Cross-contract wiring"
has "$TMP/pm"       "$NFT"      && ok "PolicyManager -> PolicyNft"       || bad "PolicyManager is missing its PolicyNft reference"
has "$TMP/nft"      "$PM"       && ok "PolicyNft -> PolicyManager"       || bad "PolicyNft is missing its PolicyManager reference"
has "$TMP/pr"       "$PM"       && ok "PayoutReceiver -> PolicyManager"  || bad "PayoutReceiver is missing its PolicyManager reference"
# The PayoutReceiver <-> SettlementAdapter binding, asked through the exact views. Both
# directions must hold: an adapter that names another receiver refuses every obligation (500).
got="$(view "$PR" settlement_adapter)"
[[ "$got" == "$ADAPTER" ]] && ok "PayoutReceiver.settlement_adapter() == SettlementAdapter" \
                           || bad "PayoutReceiver.settlement_adapter() is '${got:-<empty>}', expected $ADAPTER"
got="$(view "$ADAPTER" payout_receiver)"
[[ "$got" == "$PR" ]] && ok "SettlementAdapter.payout_receiver() == PayoutReceiver" \
                      || bad "SettlementAdapter.payout_receiver() is '${got:-<empty>}', expected $PR. Every determination will revert (500). Run set_payout_receiver."
echo

echo "3. Roles (the ones whose absence kills the money path)"
# Exact queries against each contract's has_role view. Two of these are held by CONTRACTS,
# not people - the set most often missed, and none of them fails at deploy time.
role_is() {                     # role_is <label> <contract> <Role> <who> <consequence>
  local label="$1" contract="$2" role="$3" who="$4" why="$5" out
  # Role is a #[repr(u32)] enum: the CLI takes its number (crates/shared/src/types.rs).
  case "$role" in
    Admin) role=0 ;; Backend) role=1 ;; Oracle) role=2 ;;
    Relayer) role=4 ;; Upgrader) role=5 ;; Minter) role=6 ;; Funder) role=7 ;;
  esac
  out="$(stellar contract invoke --id "$contract" --network "$NETWORK" --source "${ADMIN_KEY:-default}" \
          -- has_role --role "$role" --who "$who" 2>/dev/null | tr -d '"' | tail -1)"
  case "$out" in
    true)  ok "$label" ;;
    false) bad "$label - NOT GRANTED. $why" ;;
    *)     bad "$label - could not query has_role (got: '${out:-<empty>}')" ;;
  esac
}

role_is "PolicyManager holds Minter on PolicyNft"      "$NFT"      Minter  "$PM" \
        "Policy activation cannot mint a certificate."
role_is "PayoutReceiver holds Oracle on PolicyManager" "$PM"       Oracle  "$PR" \
        "Determinations cannot mark a policy claimed."
if [[ -n "$FUNDER_ADDR" ]]; then
  role_is "funder holds Funder on SettlementAdapter" "$ADAPTER" Funder "$FUNDER_ADDR" \
          "obligations can never be marked funded/settled."
else
  bad "set FUNDER_ADDR (or BACKEND_ADDR) to check the Funder role on the SettlementAdapter"
fi

if [[ -n "${BACKEND_ADDR:-}" ]]; then
  role_is "backend holds Backend on PolicyManager" "$PM"       Backend "$BACKEND_ADDR" \
          "The backend cannot create policies."
else
  echo "  (set BACKEND_ADDR to also check the backend's roles)"
fi
if [[ -n "${RELAYER_ADDR:-}" ]]; then
  role_is "relayer holds Relayer on PayoutReceiver" "$PR" Relayer "$RELAYER_ADDR" \
          "Determinations cannot be submitted."
else
  echo "  (set RELAYER_ADDR to also check the relayer role)"
fi
echo

echo "4. Payout authority"
if [[ -n "$DOMAIN" ]]; then
  has "$TMP/pr" "$DOMAIN" && ok "network_domain set and matches state: $DOMAIN" \
                          || bad "network_domain in state ($DOMAIN) not found on PayoutReceiver"
else
  bad "no network_domain recorded in the state file"
fi
if [[ -n "${AUTHORIZED_SIGNER_PUBKEY:-}" ]]; then
  has "$TMP/pr" "${AUTHORIZED_SIGNER_PUBKEY#0x}" && ok "authorized signer matches the expected PKP pubkey" \
                                                 || bad "authorized signer does NOT match AUTHORIZED_SIGNER_PUBKEY"
else
  echo "  (set AUTHORIZED_SIGNER_PUBKEY to also check the signer)"
fi

echo
echo "5. Determination methodologies (MethodologyKind: 0 = CROP_DAMAGE, 1 = CROP_DROUGHT)"
methodology() {                 # methodology <kind> -> true|false|<error>
  stellar contract invoke --id "$PR" --network "$NETWORK" --source "${ADMIN_KEY:-default}" \
      --send no -- is_methodology_enabled --kind "$1" 2>/dev/null | tr -d '"' | tail -1
}
case "$(methodology 1)" in
  true)  ok "CROP_DROUGHT (crop-drought-rdi-1.0) enabled" ;;
  false) bad "CROP_DROUGHT is DISABLED - no determination can settle. Run set_methodology_enabled --kind 1 --enabled true" ;;
  *)     bad "could not query is_methodology_enabled (contract older than the drought upgrade?)" ;;
esac
case "$(methodology 0)" in
  false) ok "CROP_DAMAGE (crop-dualindex-1.0, retired) disabled" ;;
  true)  bad "CROP_DAMAGE is ENABLED - the retired methodology should stay off" ;;
  *)     bad "could not query is_methodology_enabled for CROP_DAMAGE" ;;
esac

echo
echo "6. Rent: every instance and code entry lives at least 23 days past now"
# The keeper (extend_contract_ttl, sent with --send=yes) extends below 23 days to 30 days.
# Ordinary calls only top up below 3 days to 10 days, so anything under 23 days here means
# the keeper did not run or was never submitted, and the next user call pays code rent.
MIN_TTL_LEDGERS=$((23 * 17280))
ttl_left() {                    # ttl_left <ledger entry fetch args...> -> ledgers left | ''
  stellar ledger entry fetch "$@" --network "$NETWORK" --output json 2>/dev/null \
    | jq -r '(.entries[0].liveUntilLedgerSeq // empty) as $l | if $l == "" then empty else ($l - .latestLedger) end' 2>/dev/null
}
for pair in "${CONTRACTS[@]}"; do
  name="${pair%%:*}"; addr="${pair#*:}"
  [[ -n "$addr" ]] || continue
  inst_json="$(stellar ledger entry fetch contract-data --contract "$addr" --instance --network "$NETWORK" --output json 2>/dev/null || true)"
  hash="$(jq -r '.entries[0].val.contract_data.val.contract_instance.executable.wasm // empty' <<<"$inst_json" 2>/dev/null || true)"
  left="$(jq -r '(.entries[0].liveUntilLedgerSeq // empty) as $l | if $l == "" then empty else ($l - .latestLedger) end' <<<"$inst_json" 2>/dev/null || true)"
  if [[ -z "$left" ]]; then
    bad "$name instance TTL could not be read"
  elif (( left >= MIN_TTL_LEDGERS )); then
    ok "$name instance lives $((left / 17280)) more days"
  else
    bad "$name instance has only $((left / 17280)) days left (< 23). Run the keeper: extend_contract_ttl with --send=yes"
  fi
  if [[ -z "$hash" ]]; then
    bad "$name code hash could not be read from its instance"; continue
  fi
  echo "$hash" > "$TMP/hash_$name"
  left="$(ttl_left contract-code --wasm-hash "$hash")"
  if [[ -z "$left" ]]; then
    bad "$name code entry TTL could not be read ($hash)"
  elif (( left >= MIN_TTL_LEDGERS )); then
    ok "$name code lives $((left / 17280)) more days"
  else
    bad "$name code has only $((left / 17280)) days left (< 23). Run the keeper: extend_contract_ttl with --send=yes"
  fi
done

echo
echo "7. Deployed code is the local build (stand-in for SEP-55 attestation)"
# Each instance's wasm hash must equal sha256 of the local optimized wasm deploy.sh uploaded.
# This proves the chain runs the bytes in target/, not that target/ came from a given commit:
# that needs the SEP-55 release workflow (MIGRATION_NOTES §11, flag 1).
WASM_DIR="target/wasm32v1-none/release"
for pair in "PolicyNft:microcrop_policy_nft" "PolicyManager:microcrop_policy_manager" \
            "SettlementAdapter:microcrop_settlement_adapter" "PayoutReceiver:microcrop_payout_receiver"; do
  name="${pair%%:*}"; wasm="$WASM_DIR/${pair#*:}.optimized.wasm"
  if [[ ! -f "$TMP/hash_$name" ]]; then bad "$name: no on-chain hash to compare (see section 6)"; continue; fi
  if [[ ! -f "$wasm" ]]; then bad "$name: $wasm not found locally"; continue; fi
  onchain="$(tr 'A-F' 'a-f' < "$TMP/hash_$name")"
  local_hash="$(shasum -a 256 "$wasm" | cut -d' ' -f1)"
  if [[ "$onchain" == "$local_hash" ]]; then
    ok "$name wasm $local_hash"
  else
    bad "$name runs $onchain but the local build is $local_hash"
  fi
done

echo
echo "=============================================="
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  cat <<'EOF'

Some checks failed. Re-running deploy.sh is SAFE and resumable: it skips every step already
recorded in the state file. If a step is recorded but the on-chain state disagrees, delete
that key from the state file and re-run to redo just that step.
EOF
  exit 1
fi
echo "Deployment verified."
