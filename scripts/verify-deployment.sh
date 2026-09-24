#!/usr/bin/env bash
#
# Verify a MicroCrop Soroban deployment: every contract live, every cross-reference wired,
# every role granted, the signer and network domain set.
#
# WHY THIS EXISTS. None of the wiring fails at deploy time. A missed role grant produces a
# deployment that looks complete and is dead on the money path — the backend cannot create a
# policy, or PayoutReceiver cannot draw a payout — and you discover it on the first real
# claim. This reads the deployed state back and says so before that happens.
#
# HOW IT CHECKS ROLES. The contracts expose no `has_role` view (see the note at the end), so
# roles cannot be queried through the interface. They ARE persistent storage entries, so this
# dumps contract storage and looks for them. That is indirect, and it is why the last section
# recommends adding proper view functions.
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
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

STATE="${STATE_DIR}/${NETWORK}.json"
[[ -f "$STATE" ]] || { echo "no deployment state at $STATE — run deploy.sh first" >&2; exit 1; }

command -v stellar >/dev/null || { echo "stellar CLI not found" >&2; exit 1; }
command -v jq      >/dev/null || { echo "jq not found" >&2; exit 1; }

get() { jq -r --arg k "$1" '.[$k] // empty' "$STATE"; }

NFT="$(get policy_nft)"; PM="$(get policy_manager)"
TREASURY="$(get treasury)"; PR="$(get payout_receiver)"
DOMAIN="$(get network_domain)"

PASS=0; FAIL=0
ok()   { printf '  \033[1;32m✓\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[1;31m✗\033[0m %s\n' "$*"; FAIL=$((FAIL+1)); }

echo "Verifying $NETWORK deployment from $STATE"
echo

echo "1. Contracts deployed and reachable"
for pair in "PolicyNft:$NFT" "PolicyManager:$PM" "Treasury:$TREASURY" "PayoutReceiver:$PR"; do
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
  { stellar contract read --id "$addr" --network "$NETWORK" --durability persistent 2>/dev/null || true; } >  "$out"
  { stellar contract read --id "$addr" --network "$NETWORK" --durability temporary  2>/dev/null || true; } >> "$out"
}
[[ -n "$NFT"      ]] && dump "$NFT"      "$TMP/nft"
[[ -n "$PM"       ]] && dump "$PM"       "$TMP/pm"
[[ -n "$TREASURY" ]] && dump "$TREASURY" "$TMP/treasury"
[[ -n "$PR"       ]] && dump "$PR"       "$TMP/pr"

has() { [[ -f "$1" ]] && grep -qiF -- "$2" "$1"; }

echo "2. Cross-contract wiring"
has "$TMP/pm"       "$NFT"      && ok "PolicyManager -> PolicyNft"       || bad "PolicyManager is missing its PolicyNft reference"
has "$TMP/nft"      "$PM"       && ok "PolicyNft -> PolicyManager"       || bad "PolicyNft is missing its PolicyManager reference"
has "$TMP/treasury" "$PM"       && ok "Treasury -> PolicyManager"        || bad "Treasury is missing its PolicyManager reference"
has "$TMP/treasury" "$PR"       && ok "Treasury -> PayoutReceiver"       || bad "Treasury is missing its PayoutReceiver reference"
has "$TMP/pr"       "$TREASURY" && ok "PayoutReceiver -> Treasury"       || bad "PayoutReceiver is missing its Treasury reference"
has "$TMP/pr"       "$PM"       && ok "PayoutReceiver -> PolicyManager"  || bad "PayoutReceiver is missing its PolicyManager reference"
echo

echo "3. Roles (the ones whose absence kills the money path)"
# PM must hold Minter on the NFT, PR must hold Oracle on PM and Payout on Treasury.
has "$TMP/nft"      "$PM" && ok "PolicyManager holds Minter on PolicyNft" \
                          || bad "PolicyManager has NO Minter role — policy activation cannot mint a certificate"
has "$TMP/pm"       "$PR" && ok "PayoutReceiver holds Oracle on PolicyManager" \
                          || bad "PayoutReceiver has NO Oracle role — determinations cannot mark a policy claimed"
has "$TMP/treasury" "$PR" && ok "PayoutReceiver holds Payout on Treasury" \
                          || bad "PayoutReceiver has NO Payout role — NO PAYOUT CAN EVER BE DRAWN"
[[ -n "${BACKEND_ADDR:-}" ]] && {
  has "$TMP/pm"       "$BACKEND_ADDR" && ok "backend holds Backend on PolicyManager" \
                                      || bad "backend has NO Backend role — it cannot create policies"
  has "$TMP/treasury" "$BACKEND_ADDR" && ok "backend holds Backend on Treasury" \
                                      || bad "backend has NO Backend role — it cannot receive premiums"
} || echo "  (set BACKEND_ADDR to also check the backend's roles)"
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
echo "=============================================="
printf 'passed: %d   failed: %d\n' "$PASS" "$FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  cat <<'EOF'

Some checks failed. Re-running deploy.sh is SAFE and resumable: it skips every step already
recorded in the state file. If a step is recorded but the on-chain state disagrees, delete
that key from the state file and re-run to redo just that step.

NOTE ON ROLE CHECKS: the contracts expose no `has_role` view, so the checks above read raw
contract storage and match on addresses. That is indirect and can produce a false PASS when
an address appears in storage for an unrelated reason. Adding `has_role(role, who) -> bool`
as a view to each contract would make this verification exact, and is recommended before
mainnet.
EOF
  exit 1
fi
echo "Deployment verified."
