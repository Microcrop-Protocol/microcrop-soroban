#!/usr/bin/env bash
#
# MicroCrop SettlementAdapter evidence run — TESTNET ONLY (Option B, MIGRATION_NOTES §11).
#
# WHY TWO LANES. A positive drought determination on the real PolicyManager needs 110+ days
# of ledger time after the sale (activate_policy sets start_date = now; the season window is
# 110 days; the determination must be fresh). So:
#
#   Lane P (production contracts, deployed by deploy.sh): a real sale on the real
#     PolicyManager, then a determination that MUST stop at #327 SeasonNotComplete. 327 fires
#     before signature recovery, so this proves validation steps 1-11 run on the live stack
#     without touching the PKP signer.
#
#   Lane E (evidence lane): the SAME SettlementAdapter and PayoutReceiver WASM (hashes are
#     compared with Lane P's) wired to TestnetPolicyFixture, a PolicyManager stand-in whose
#     admin INJECTS a policy (not a sale). A real historical season (2022 ERA5 long rains,
#     1.0167 N / 35.0062 E) is signed by a THROWAWAY key that only this lane's PayoutReceiver
#     trusts, and runs end to end: ObligationRecorded (Owed) -> mark_funded and mark_settled
#     by the backend (Funder), plus the negative cases (including the org refused when it
#     tries to mark its own obligation settled).
#
# Never uses the production PKP or the Lit account. Refuses --network mainnet.
#
# Usage:
#   ./scripts/testnet-evidence.sh --lane-p-state <dir>/testnet.json --state-dir <dir>/evidence
#   ./scripts/testnet-evidence.sh ... --lane P      # or E, or all (default)
#
# Environment (stellar CLI identity NAMES; created and funded by friendbot when missing):
#   ADMIN_KEY    holds Admin on the Lane P contracts (deploy.sh's ADMIN_KEY); admin of Lane E
#   BACKEND_KEY  holds Backend on Lane P's PolicyManager; Funder on Lane E's adapter
#   RELAYER_KEY  holds Relayer on Lane P's PayoutReceiver; Relayer on Lane E's
#   ORG_KEY      the org; it signs a mark_settled that must be refused (default: evidence-org)
#   STRANGER_KEY an account with no role (default: evidence-stranger)
#   EVIDENCE_SIGNER_SK  optional 32-byte hex throwaway secp256k1 key (generated if unset)
#
set -euo pipefail

NETWORK="testnet"
LANE="all"
LANE_P_STATE=""
STATE_DIR=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --network) NETWORK="${2:?}"; shift 2 ;;
    --lane) LANE="${2:?}"; shift 2 ;;
    --lane-p-state) LANE_P_STATE="${2:?}"; shift 2 ;;
    --state-dir) STATE_DIR="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

say()  { printf '\033[1;36m[evidence]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[fatal]\033[0m %s\n' "$*" >&2; exit 1; }

[[ "$NETWORK" == "testnet" ]] || die "testnet only (got --network $NETWORK). The fixture refuses mainnet too."
[[ -n "$LANE_P_STATE" && -f "$LANE_P_STATE" ]] || die "--lane-p-state <deploy.sh state file> is required"
[[ -n "$STATE_DIR" ]] || die "--state-dir is required (its own directory, never deploy.sh's)"
[[ "$(cd "$(dirname "$LANE_P_STATE")" && pwd)" != "$(mkdir -p "$STATE_DIR" && cd "$STATE_DIR" && pwd)" ]] \
  || die "--state-dir must differ from the Lane P state directory"
command -v stellar >/dev/null || die "stellar CLI not found"
command -v jq      >/dev/null || die "jq not found"
command -v cargo   >/dev/null || die "cargo not found"

PASSPHRASE="Test SDF Network ; September 2015"
DOMAIN="$(printf '%s' "$PASSPHRASE" | shasum -a 256 | cut -d' ' -f1)"
STATE="$STATE_DIR/evidence.json"
LOGS="$STATE_DIR/logs"
REPORT="$STATE_DIR/evidence.md"
mkdir -p "$LOGS"
[[ -f "$STATE" ]] || echo '{}' > "$STATE"

state_get() { jq -r --arg k "$1" '.[$k] // empty' "$STATE"; }
state_set() { local t; t="$(mktemp)"; jq --arg k "$1" --arg v "$2" '.[$k] = $v' "$STATE" > "$t" && mv "$t" "$STATE"; }
lp_get()    { jq -r --arg k "$1" '.[$k] // empty' "$LANE_P_STATE"; }

[[ -z "$(lp_get treasury)" ]] || die "Lane P state is a Treasury-era deployment; deploy Option B fresh first"
[[ "$(lp_get network_domain)" == "$DOMAIN" ]] || die "Lane P network_domain is not sha256(testnet passphrase)"
P_PM="$(lp_get policy_manager)"; P_PR="$(lp_get payout_receiver)"; P_AD="$(lp_get settlement_adapter)"
[[ -n "$P_PM" && -n "$P_PR" && -n "$P_AD" ]] || die "Lane P state is missing a contract"

: "${ADMIN_KEY:?ADMIN_KEY is required}"
: "${BACKEND_KEY:?BACKEND_KEY is required}"
: "${RELAYER_KEY:?RELAYER_KEY is required}"
ORG_KEY="${ORG_KEY:-evidence-org}"
STRANGER_KEY="${STRANGER_KEY:-evidence-stranger}"
FARMER_KEY="${FARMER_KEY:-evidence-farmer}"

ensure_identity() {
  stellar keys address "$1" >/dev/null 2>&1 \
    || stellar keys generate "$1" --network "$NETWORK" --fund >/dev/null 2>&1 \
    || die "could not create identity $1"
  stellar keys address "$1"
}
ADMIN="$(ensure_identity "$ADMIN_KEY")"
BACKEND="$(ensure_identity "$BACKEND_KEY")"
RELAYER="$(ensure_identity "$RELAYER_KEY")"
ORG="$(ensure_identity "$ORG_KEY")"
STRANGER="$(ensure_identity "$STRANGER_KEY")"
FARMER="$(ensure_identity "$FARMER_KEY")"

# The throwaway signer. Stored in this lane's own state so a resumed run signs with the same
# key the evidence PayoutReceiver was configured with.
SK="${EVIDENCE_SIGNER_SK:-$(state_get signer_sk)}"
if [[ -z "$SK" ]]; then SK="$(openssl rand -hex 32)"; fi
state_set signer_sk "$SK"
signer() { cargo run -q -p microcrop-integration-tests --example evidence_signer -- "$@"; }
SIGNER_PUBKEY="$(signer pubkey "$SK")"

# The evidence: plot and sum insured shared by both lanes.
LAT=1016700; LON=35006200
SUM_INSURED=10000000000      # 1,000 USDC, 7dp
PREMIUM=1000000000           # recorded on the policy only; never paid on-chain
PLANT_2022=1648771200        # 2022-04-01 00:00 UTC

# ---------------------------------------------------------------------------
# Invocation helpers: every call is logged; tx hashes go into the report.
# ---------------------------------------------------------------------------
report() { printf '%s\n' "$*" >> "$REPORT"; }
tx_line() { [[ -n "$2" ]] && report "| $1 | \`$2\` | https://stellar.expert/explorer/testnet/tx/$2 |" || report "| $1 | (simulation only) | |"; }

LAST_OUT=""; LAST_HASH=""
inv() {                          # inv <label> <source> <contract> [cli flags...] -- fn args...
  local label="$1" src="$2" id="$3"; shift 3
  local extra=(); while [[ "$1" != "--" ]]; do extra+=("$1"); shift; done
  local rc=0
  LAST_OUT="$(stellar contract invoke --id "$id" --source "$src" --network "$NETWORK" \
      ${extra[@]+"${extra[@]}"} "$@" 2>&1)" || rc=$?
  printf '%s\n' "$LAST_OUT" > "$LOGS/$label.log"
  LAST_HASH="$(printf '%s' "$LAST_OUT" | { grep -o 'Signing transaction: [0-9a-f]*' || true; } | tail -1 | awk '{print $3}')"
  return "$rc"
}
ok_inv() {                       # like inv, but must succeed; records the tx
  local label="$1"
  inv "$@" || { tail -5 "$LOGS/$label.log" >&2; die "$label failed"; }
  tx_line "$label" "$LAST_HASH"
  say "$label ✓ ${LAST_HASH:-}"
}
expect_err() {                   # expect_err <code> <label> <source> <contract> ... -- fn args
  local code="$1"; shift
  local label="$1"
  if inv "$@"; then die "$label unexpectedly succeeded (expected #$code)"; fi
  grep -q "Error(Contract, #$code)" "$LOGS/$label.log" \
    || { tail -5 "$LOGS/$label.log" >&2; die "$label failed, but not with #$code"; }
  report "| $label | refused **#$code** | (simulation) |"
  say "$label refused with #$code ✓"
}
expect_auth_err() {              # the host refuses a missing signature: not a contract error
  local label="$1"
  if inv "$@"; then die "$label unexpectedly succeeded (expected an auth failure)"; fi
  if grep -q "Error(Contract" "$LOGS/$label.log"; then
    tail -5 "$LOGS/$label.log" >&2; die "$label failed with a contract error, expected an auth failure"
  fi
  report "| $label | ${AUTH_ERR_NOTE:-refused (missing authorization)} | (simulation) |"
  say "$label refused: missing authorization ✓"
}
value() { printf '%s' "$LAST_OUT" | tail -1 | tr -d '"'; }

wasm_hash() {                    # the wasm hash an instance runs
  stellar ledger entry fetch contract-data --contract "$1" --instance --network "$NETWORK" --output json 2>/dev/null \
    | jq -r '.entries[0].val.contract_data.val.contract_instance.executable.wasm // empty' | tr 'A-F' 'a-f'
}

WASM_DIR="target/wasm32v1-none/release"
deploy() {                       # deploy <state-key> <wasm-name> [--ctor-args...]
  local key="$1" wasm="$2"; shift 2
  local existing; existing="$(state_get "$key")"
  if [[ -n "$existing" ]]; then echo "$existing"; return; fi
  local addr
  addr="$(stellar contract deploy --wasm "$WASM_DIR/$wasm.optimized.wasm" \
      --source "$ADMIN_KEY" --network "$NETWORK" -- "$@" | tail -1)"
  [[ "$addr" =~ ^C[A-Z0-9]{55}$ ]] || die "$key deploy returned: $addr"
  state_set "$key" "$addr"
  say "$key deployed: $addr"
  echo "$addr"
}
step() {                         # step <state-key> <contract> <fn> [--args...] (admin, once)
  local key="$1" contract="$2" fn="$3"; shift 3
  [[ -z "$(state_get "$key")" ]] || return 0
  ok_inv "$key" "$ADMIN_KEY" "$contract" -- "$fn" --caller "$ADMIN" "$@"
  state_set "$key" "done"
}
ref() { printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1; }

: > "$REPORT"
report "# SettlementAdapter testnet evidence ($(date -u +%Y-%m-%dT%H:%MZ))"
report ""
report "Lane P state: \`$LANE_P_STATE\`. Network domain \`$DOMAIN\`. Throwaway signer pubkey \`$SIGNER_PUBKEY\` (Lane E only; not the PKP)."
report ""

# ---------------------------------------------------------------------------
# Lane P: real sale on the real PolicyManager, determination stops at #327
# ---------------------------------------------------------------------------
lane_p() {
  report "## Lane P: production contracts"
  report ""
  report "PolicyManager \`$P_PM\`, SettlementAdapter \`$P_AD\`, PayoutReceiver \`$P_PR\`."
  report ""
  report "| step | tx / result | link |"
  report "|---|---|---|"

  local pid; pid="$(state_get p_policy_id)"
  if [[ -z "$pid" ]]; then
    ok_inv p_create_policy "$BACKEND_KEY" "$P_PM" -- create_policy --caller "$BACKEND" \
        --farmer "$FARMER" --plot_id 1 --lat_e6 "$LAT" --lon_e6 "$LON" \
        --sum_insured "$SUM_INSURED" --premium "$PREMIUM" --duration_days 150 \
        --coverage_type 0 --org "$ORG"
    pid="$(value)"; state_set p_policy_id "$pid"
    ok_inv p_activate_policy "$BACKEND_KEY" "$P_PM" -- activate_policy --caller "$BACKEND" \
        --policy_id "$pid" --distributor "$BACKEND" --distributor_name "Evidence run" \
        --region "Trans Nzoia"
  fi
  report "| sale | policy id **$pid** (Drought, 150 d, no premium on-chain) | |"

  # Season start = today 00:00 UTC (the cover started today); assessed a minute ago. The
  # signature is the throwaway key's: 327 fires before signature recovery.
  local now today signed
  now="$(date -u +%s)"; today=$((now - now % 86400))
  signed="$(signer sign "$SK" "$P_PR" "$DOMAIN" "$pid" "$today" "$((now - 60))" "$LAT" "$LON" "$SUM_INSURED")"
  expect_err 327 p_submit_season_not_complete "$RELAYER_KEY" "$P_PR" -- submit_drought_determination \
      --caller "$RELAYER" --d "$(jq -c .d_cli <<<"$signed")" \
      --signature "$(jq -r .signature <<<"$signed")" --recovery_id "$(jq -r .recovery_id <<<"$signed")"
  inv p_obligation_count "$ADMIN_KEY" "$P_AD" --send no -- obligation_count || true
  report "| Lane P adapter obligation_count | $(value) | |"
  report ""
}

# ---------------------------------------------------------------------------
# Lane E: fixture + the same adapter / PayoutReceiver wasm, a real 2022 season
# ---------------------------------------------------------------------------
lane_e() {
  say "building the fixture wasm"
  cargo build -q -p microcrop-testnet-policy-fixture --target wasm32v1-none --release
  stellar contract optimize --wasm "$WASM_DIR/microcrop_testnet_policy_fixture.wasm" >/dev/null 2>&1
  for w in microcrop_settlement_adapter microcrop_payout_receiver; do
    [[ -f "$WASM_DIR/$w.optimized.wasm" ]] || die "missing $WASM_DIR/$w.optimized.wasm (run deploy.sh's build first)"
  done

  local FIX AD PR AD2
  FIX="$(deploy e_fixture microcrop_testnet_policy_fixture --admin "$ADMIN")"
  AD="$(deploy e_adapter microcrop_settlement_adapter --admin "$ADMIN")"
  PR="$(deploy e_payout_receiver microcrop_payout_receiver \
        --settlement_adapter "$AD" --policy_manager "$FIX" --admin "$ADMIN")"

  report "## Lane E: evidence lane"
  report ""
  report "TestnetPolicyFixture \`$FIX\` (injects policy state; not a sale), SettlementAdapter-E \`$AD\`, PayoutReceiver-E \`$PR\`."
  report ""
  report "| step | tx / result | link |"
  report "|---|---|---|"

  step e_wire_adapter_pr   "$AD"  set_payout_receiver --payout_receiver "$PR"
  step e_role_fix_oracle   "$FIX" grant_role --role 2 --who "$PR"
  step e_role_pr_relayer   "$PR"  grant_role --role 4 --who "$RELAYER"
  step e_role_ad_funder    "$AD"  grant_role --role 7 --who "$BACKEND"
  step e_cfg_signer        "$PR"  set_authorized_signer --pubkey "$SIGNER_PUBKEY"
  step e_cfg_domain        "$PR"  set_network_domain --domain "$DOMAIN"
  step e_cfg_drought       "$PR"  set_methodology_enabled --kind 1 --enabled true

  # The injected policy: Drought (0), Active (1), cover 2022-04-01 06:00 UTC to 2027-01-01.
  policy_json() {                # policy_json <status>
    jq -nc --arg farmer "$FARMER" --arg org "$ORG" --argjson st "$1" \
      --argjson lat "$LAT" --argjson lon "$LON" --arg si "$SUM_INSURED" --arg pr "$PREMIUM" \
      '{id:1, farmer:$farmer, plot_id:1, lat_e6:$lat, lon_e6:$lon, sum_insured:$si, premium:$pr,
        start_date:1648792800, end_date:1798761600, coverage_type:0, status:$st,
        created_at:1648792800, org:$org}'
  }
  step e_set_policy "$FIX" set_policy --policy "$(policy_json 1)"

  # Sign the real 2022 season now (fresh: assessed a minute ago), and check the contract
  # rebuilds the same digest as the local encoder.
  local now signed d sig rid onchain
  now="$(date -u +%s)"
  signed="$(signer sign "$SK" "$PR" "$DOMAIN" 1 "$PLANT_2022" "$((now - 60))" "$LAT" "$LON" "$SUM_INSURED")"
  d="$(jq -c .d_cli <<<"$signed")"; sig="$(jq -r .signature <<<"$signed")"; rid="$(jq -r .recovery_id <<<"$signed")"
  inv e_digest_view "$ADMIN_KEY" "$PR" --send no -- drought_determination_digest --d "$d" \
    || die "drought_determination_digest view failed"
  onchain="$(value)"
  [[ "$onchain" == "$(jq -r .digest <<<"$signed")" ]] || die "on-chain digest $onchain != local encoder"
  report "| digest view == local encoder | \`$onchain\` | |"

  if [[ -z "$(state_get e_submitted)" ]]; then
    ok_inv e_submit_drought_determination "$RELAYER_KEY" "$PR" -- submit_drought_determination \
        --caller "$RELAYER" --d "$d" --signature "$sig" --recovery_id "$rid"
    state_set e_submitted "$LAST_HASH"
  fi

  inv e_find_obligation "$ADMIN_KEY" "$AD" --send no -- find_obligation --payout_receiver "$PR" --policy_id 1
  local id; id="$(value)"; [[ "$id" == "1" ]] || die "find_obligation returned '$id'"
  inv e_get_obligation "$ADMIN_KEY" "$AD" --send no -- get_obligation --obligation_id 1
  report "| get_obligation(1) after submit | \`$(value)\` | |"
  inv e_fixture_policy "$ADMIN_KEY" "$FIX" --send no -- get_policy --policy_id 1
  [[ "$(printf '%s' "$LAST_OUT" | tail -1 | jq -r .status)" == "4" ]] || die "fixture policy is not Claimed"
  report "| fixture policy status | Claimed (4) | |"

  local rf rs
  rf="$(ref "microcrop:evidence:funding:v1:$(date -u +%F)")"
  rs="$(ref "microcrop:evidence:settlement:v1:$(date -u +%F)")"
  if [[ -z "$(state_get e_funded)" ]]; then
    ok_inv e_mark_funded_by_funder "$BACKEND_KEY" "$AD" -- mark_funded --caller "$BACKEND" \
        --obligation_id 1 --reference_hash "$rf"
    state_set e_funded "$LAST_HASH"
  fi
  if [[ -z "$(state_get e_settled)" ]]; then
    # The org owes the obligation: signing its own mark_settled is refused (#1 Unauthorized).
    expect_err 1 e_neg_org_marks_own_settled "$ORG_KEY" "$AD" -- mark_settled --caller "$ORG" \
        --obligation_id 1 --reference_hash "$rs"
    ok_inv e_mark_settled_by_funder "$BACKEND_KEY" "$AD" -- mark_settled --caller "$BACKEND" \
        --obligation_id 1 --reference_hash "$rs"
    state_set e_settled "$LAST_HASH"
  fi
  inv e_get_obligation_settled "$ADMIN_KEY" "$AD" --send no -- get_obligation --obligation_id 1
  report "| get_obligation(1) after settle | \`$(value)\` | |"

  # ---- negative cases ----
  expect_err 1   e_neg_stranger_mark "$STRANGER_KEY" "$AD" -- mark_funded --caller "$STRANGER" \
      --obligation_id 1 --reference_hash "$rf"
  # CLI simulation RECORDS auth rather than enforcing it, so this fails client-side: no key can
  # sign for a contract address. The host-side refusal is proven by the unit test
  # record_needs_the_receivers_own_authorization; the report says exactly that.
  AUTH_ERR_NOTE="no key can produce PayoutReceiver authorization (contract address); host refusal proven by unit test record_needs_the_receivers_own_authorization" \
  expect_auth_err e_neg_record_without_pr_auth "$ADMIN_KEY" "$AD" -- record_obligation --caller "$PR" \
      --policy_id 2 --org "$ORG" --amount 1 --determination_digest "$rf"
  expect_err 506 e_neg_zero_reference "$BACKEND_KEY" "$AD" -- mark_funded --caller "$BACKEND" \
      --obligation_id 1 --reference_hash "$(printf '0%.0s' $(seq 1 64))"
  # Replay: the fixture marked the policy Claimed, so first 308; re-inject it as Active and the
  # PayoutReceiver's own PolicyPaid guard answers 310.
  if [[ -z "$(state_get e_reset_policy_active)" ]]; then
    expect_err 308 e_neg_replay_claimed "$RELAYER_KEY" "$PR" -- submit_drought_determination \
        --caller "$RELAYER" --d "$d" --signature "$sig" --recovery_id "$rid"
  fi
  step e_reset_policy_active "$FIX" set_policy --policy "$(policy_json 1)"
  expect_err 310 e_neg_replay_paid "$RELAYER_KEY" "$PR" -- submit_drought_determination \
      --caller "$RELAYER" --d "$d" --signature "$sig" --recovery_id "$rid"
  # An adapter bound to another PayoutReceiver (Lane P's) is refused.
  expect_err 328 e_neg_adapter_bound_elsewhere "$ADMIN_KEY" "$PR" -- set_settlement_adapter \
      --caller "$ADMIN" --adapter "$P_AD"
  # Adapter-2 bound to PayoutReceiver-E is accepted, and the replay is still refused (310).
  AD2="$(deploy e_adapter2 microcrop_settlement_adapter --admin "$ADMIN")"
  step e_wire_adapter2_pr "$AD2" set_payout_receiver --payout_receiver "$PR"
  step e_swap_adapter     "$PR"  set_settlement_adapter --adapter "$AD2"
  expect_err 310 e_neg_replay_after_swap "$RELAYER_KEY" "$PR" -- submit_drought_determination \
      --caller "$RELAYER" --d "$d" --signature "$sig" --recovery_id "$rid"
  inv e_adapter2_count "$ADMIN_KEY" "$AD2" --send no -- obligation_count || true
  report "| Adapter-2 obligation_count after the replay | $(value) | |"

  # ---- same code as Lane P ----
  local hp ha hpe hae
  hp="$(wasm_hash "$P_PR")"; ha="$(wasm_hash "$P_AD")"
  hpe="$(wasm_hash "$PR")";  hae="$(wasm_hash "$AD")"
  [[ -n "$hp" && "$hp" == "$hpe" ]] || die "PayoutReceiver-E wasm $hpe != Lane P $hp"
  [[ -n "$ha" && "$ha" == "$hae" ]] || die "SettlementAdapter-E wasm $hae != Lane P $ha"
  report ""
  report "Identical code: PayoutReceiver wasm \`$hp\` and SettlementAdapter wasm \`$ha\` in both lanes."
  report ""
}

case "$LANE" in
  P|p) lane_p ;;
  E|e) lane_e ;;
  all) lane_p; lane_e ;;
  *) die "--lane must be P, E or all" ;;
esac
say "report: $REPORT"
