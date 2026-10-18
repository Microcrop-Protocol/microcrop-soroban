# Deployment runbook — MicroCrop on Soroban

Deploys, wires and configures all four contracts. Two scripts do the work:

```bash
./scripts/deploy.sh --network testnet --dry-run   # print every call, change nothing
./scripts/deploy.sh --network testnet             # do it
./scripts/verify-deployment.sh --network testnet  # read the result back
```

---

## Why this is a script and not a checklist

The contracts are not independent. PayoutReceiver's constructor consumes the Treasury and
PolicyManager addresses, so deployment has a required order. Worse, **six cross-contract role
grants have to be made after deployment, and none of them fail at deploy time.** A missed
grant produces a deployment that looks complete and is dead on the money path — you find out
on the first real claim, on chain, with a farmer waiting.

The sequence in `deploy.sh` is lifted from `crates/integration-tests`, which exercises the
identical wiring against all four real contracts. It is verified, not remembered.

---

## Prerequisites

| Thing | Notes |
|---|---|
| `stellar` CLI | `cargo install --locked stellar-cli` |
| `jq` | `brew install jq` |
| Built wasm | `cargo build --release --target wasm32v1-none` — produces 4 artifacts (27–47K) |
| Funded admin identity | `stellar keys generate deployer --network testnet --fund` |
| USDC SAC address | the USDC Stellar Asset Contract for the target network. **Must report `decimals() == 7`** — the Treasury constructor panics `UsdcDecimalsMismatch` (#217) otherwise. Check first: `stellar contract invoke --id "$USDC_SAC" --network testnet -- decimals` |

### Environment

```bash
export ADMIN_KEY=deployer              # stellar CLI identity; holds Admin+Upgrader on all four
export USDC_SAC=C...                   # USDC Stellar Asset Contract
export BACKEND_WALLET=G...             # receives platform fees
export BACKEND_ADDR=G...               # backend signs policy/premium calls with this
export RELAYER_ADDR=G...               # allowed to submit determinations (anti-spam gate)
export AUTHORIZED_SIGNER_PUBKEY=04...  # 65-byte SEC-1 pubkey of the Lit PKP — 130 hex chars
```

> **`AUTHORIZED_SIGNER_PUBKEY` is a public KEY, not an Ethereum address.** The contract
> recovers a pubkey from the signature and compares it to this value, so passing
> `0x53fd0ad7…` silently fails every signature check. The script rejects anything that is not
> 130 hex characters beginning `04`. This is the single easiest way to produce a deployment
> that verifies nothing.

---

## `NETWORK_DOMAIN`

Replaces EVM `block.chainid` as the replay-domain separator. **The script derives it as
`sha256(network passphrase)`:**

| Network | `NETWORK_DOMAIN` |
|---|---|
| testnet | `cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472` |
| mainnet | `7ac33997544e3175d266bd022439b22cdb16508c01163f26e5cb2a3e1045a979` |

Why this derivation: it does what `chainid` did — binds a signed determination to exactly one
network, so a testnet signature can never be replayed on mainnet — and both sides can compute
it from a public constant, so no magic number has to be shipped between the signer and the
contract.

**The off-chain Lit PKP signer must use the identical value.** If the two disagree by one
byte, every determination fails verification, and the failure looks like a bad signature
rather than a configuration mismatch. Override with `NETWORK_DOMAIN=…` only if the signer has
already committed to something else.

---

## What the script does, in order

**Deploy** (order forced by constructor arguments)

1. `PolicyNft(admin, "MicroCrop Policy", "MCP")`
2. `PolicyManager(admin)`
3. `Treasury(usdc, backend_wallet, admin)` — refuses a USDC token that is not 7 decimals
4. `PayoutReceiver(treasury, policy_manager, admin)`

**Wire cross-references**

5. `PolicyManager.set_policy_nft(nft)`
6. `PolicyNft.set_policy_manager(pm)`
7. `Treasury.set_policy_manager(pm)`
8. `Treasury.set_payout_receiver(pr)`

**Grant roles** — the silent-failure section

| # | Grant | Without it |
|---|---|---|
| 9 | `PolicyManager` → Backend → backend | backend cannot create policies |
| 10 | `Treasury` → Backend → backend | backend cannot receive premiums |
| 11 | `PolicyNft` → Minter → **PolicyManager** | activation cannot mint a certificate |
| 12 | `PolicyManager` → Oracle → **PayoutReceiver** | determinations cannot mark a policy claimed |
| 13 | `Treasury` → Payout → **PayoutReceiver** | **no payout can ever be drawn** |
| 14 | `PayoutReceiver` → Relayer → relayer | determinations cannot be submitted |

Note 11–13 grant roles to *contracts*, not people. That is the part most often missed.

**Configure the payout authority**

15. `PayoutReceiver.set_authorized_signer(pubkey)`
16. `PayoutReceiver.set_network_domain(domain)`
17. `PayoutReceiver.set_methodology_enabled(kind = 1, true)` — CROP_DROUGHT. Absent means
    disabled, so nothing settles until this runs. Kind 0 (CROP_DAMAGE) is retired: leave it off.

**Keep the contracts alive**

18. `PayoutReceiver.extend_contract_ttl()` — extends all four contracts' instance and code
    to 30 days. A fresh upload sits at the network minimum (7 days on testnet) and admin calls
    extend nothing, so run it right after the deploy; the backend keeper then runs it daily
    (see *Storage rent: the keeper* below).

---

## Resuming a failed deploy

Every address and completed step is written to `deployments/<network>.json` **as soon as it
happens**. Re-running skips what is done.

**If a deploy dies halfway, run the same command again. Do not start over** — that orphans
contracts you have already paid to deploy.

To redo one step, delete its key from the state file and re-run.

---

## Verification

```bash
./scripts/verify-deployment.sh --network testnet
```

Checks all four contracts are live and reachable, all six cross-references are wired, the
money-path roles are granted, the signer and domain are set, CROP_DROUGHT is enabled and the
retired CROP_DAMAGE methodology is disabled. Exits non-zero on any failure.

Set `BACKEND_ADDR` and `AUTHORIZED_SIGNER_PUBKEY` in the environment to enable the backend
role and signer checks too.

### How the role checks work

Each contract exposes `has_role(role, who) -> bool` as a read-only view, so role checks are
**exact** — the contract is asked directly. Role membership is not secret: every grant is
already a public persistent-storage entry on a public ledger. What was missing was a way to
*ask*.

An earlier version of this script dumped raw contract storage and matched on addresses, which
was indirect and could **false-pass** when an address appeared in storage for an unrelated
reason. That is fixed.

Set `BACKEND_ADDR` and `RELAYER_ADDR` in the environment to check those grants too.

## Mainnet

`--network mainnet` requires typing a confirmation phrase.

**Before you do:** these contracts have had **no Soroban security audit**. The Solidity
originals were reviewed in-house only; the Rust port has not been reviewed at all. The
secp256k1 signature path, the replay guards and the per-org solvency logic all need fresh
review against Soroban semantics — auth trees, TTL and archival edge cases behave differently
from the EVM.

Storage TTL follows the rent cap (see **Storage rent: the keeper** below); the restore cost
of an entry that archives anyway has not been measured.

---

## Operating the contracts (behaviour after the 2026-10 audit fixes)

**Amounts are 7-decimal USDC.** 1 USDC = `10_000_000` base units. `sum_insured` must be
between `1_000_000_000` (100 USDC) and `10_000_000_000_000` (1,000,000 USDC). The backend
and the Lit signer must both scale by `1e7` on Stellar; the Base path keeps `1e6`. A signer
still on `1e6` makes every determination fail `SumInsuredMismatch`.

**Pending claims need a keeper.** When a valid determination arrives and the org's reserve
cannot pay it, the claim is not lost: Treasury records it (`PayoutPending` event,
`get_pending_payout(policy_id)`), the policy becomes `ClaimPending`, and the org cannot
withdraw that money. Once the org tops up with `deposit_reserve`, **anyone** settles it:

```bash
stellar contract invoke --id "$PR" --network testnet --source <any funded key> \
  -- settle_pending_claim --policy_id <id>
```

Watch for `PayoutPending` / `ClaimAwaitingFunding` events, alert the org, and settle after
the top-up. While an org has pending claims, a new claim is paid at once only from the
reserve not already owed to them; otherwise it is recorded as pending too. Settlement works after the policy's `end_date` and after `MAX_REPORT_AGE`; it is
blocked only while PayoutReceiver or Treasury is paused.

**Pending claims settle in order.** An org's pending claims form a first-in-first-out
queue; only the oldest settles. Ask Treasury which one that is and settle it, then repeat:

```bash
stellar contract invoke --id "$TREASURY" --network testnet --source <any key> \
  -- pending_payout_head --org <ORG_G...>      # -> policy id, or null
stellar contract invoke --id "$TREASURY" --network testnet --source <any key> \
  -- pending_payout_count --org <ORG_G...>
```

Any other id reverts `NotPendingQueueHead` (224). A keeper should loop "head, settle"
until the head's settle reverts `InsufficientOrgReserve`.

**Never switch the PayoutReceiver while a claim is pending.** Each pending claim's marker
lives in the current PayoutReceiver, so pointing Treasury at a new one would strand it and,
because settlement is first-in-first-out, block every later claim of its org.
`set_payout_receiver` therefore reverts `PayoutsStillPending` (226) while
`total_pending_payout_count()` is above zero. Settle the queues first, or upgrade the
PayoutReceiver in place (`upgrade`), which keeps its storage.

**A frozen backend wallet queues claims; rotate it.** Circle's Stellar USDC issuer has
`auth_revocable` set and `auth_clawback_enabled` off (checked on Horizon on 2026-10-13:
mainnet issuer `GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN`, testnet issuer
`GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5`; the mainnet address is not the
USDC issuer on testnet). Clawback is therefore not a risk. If the backend wallet's trustline is deauthorized, or
the wallet has no USDC trustline, a payout's transfer fails: the determination is still
accepted, Treasury emits `PayoutTransferFailed` and records the claim as pending even when
the reserve covers it. Settling into the same wallet reverts `PayoutTransferFailed` (225).
Fix: give the new wallet a USDC trustline, then

```bash
stellar contract invoke --id "$TREASURY" --network testnet --source admin \
  -- set_backend_wallet --caller <ADMIN_G...> --backend_wallet <NEW_G...>
```

and settle the queue as above. Before deploying, confirm the backend wallet holds an
authorized USDC trustline; alert on any `PayoutTransferFailed` event.

**Storage rent: the keeper.** Soroban charges rent up front for every TTL extension. The
contracts used to extend every entry, instance and code to the network maximum (~180 days)
on every touch, so the first ordinary call to each contract paid ~173 days of code rent
(362–833 XLM on testnet). They now follow a rent cap (`crates/shared/src/ttl.rs`,
MIGRATION_NOTES §10):

- ordinary calls extend instance + code only below 3 days, to 10 days (a safety net);
- admin and config calls extend nothing;
- data entries live as long as the policy needs them: `end_date + 30 days` per policy
  (clamped to the ~180-day network maximum), 60 days for pending claims and role grants,
  the longest need of any policy (at least 60 days) for per-farmer and per-org entries.

So **the backend must run a keeper**, as a repeatable job:

1. **Daily:** `extend_contract_ttl()` on PayoutReceiver. It extends PayoutReceiver and
   Treasury below 23 days to 30 days, then calls PolicyManager's, which does the same for
   PolicyManager and PolicyNft. Above 23 days it is a no-op, so running it daily costs one
   transaction fee most days and about 7 days of code rent once a week (testnet ≈ 101 XLM a
   week for all four, mainnet ≈ 12.4 XLM).
2. **For each PENDING, ACTIVE or CLAIM_PENDING policy whose shortest TTL is under 30 days:**
   `extend_policy_ttl(policy_id)` on PayoutReceiver. It extends the policy's entries in all
   four contracts to their need, never to the maximum. Needed for policies whose need is past
   the ~180-day maximum and for pending claims (60-day target).

```bash
stellar contract invoke --id "$PR" --network testnet --source <any funded key> \
  -- extend_contract_ttl
stellar contract invoke --id "$PR" --network testnet --source <any funded key> \
  -- extend_policy_ttl --policy_id <id>
```

If the keeper lapses, nothing breaks: the next user call extends the contract it touches by
up to ~7 days of code rent (testnet ≤ 34 XLM for Treasury, mainnet ≤ 4.1 XLM). CLI fallback
(the same as the keeper, without the contracts):

```bash
stellar contract extend --id C... --network testnet --source <key> \
  --durability persistent --ledgers-to-extend 518400            # instance, ~30 days
stellar contract extend --wasm-hash <hash> --network testnet --source <key> \
  --durability persistent --ledgers-to-extend 518400            # code entry
```

An entry that archives anyway is not lost and never reads as absent: it is restored
(`stellar contract restore`, or automatically by a transaction whose footprint includes it),
at a fee. Mainnet deploy budget: uploads pay the 120-day minimum (≈ 213 XLM for the four code
entries) and the keeper has nothing to do until about day 97.

```bash
# re-check the network limit (read-only; LedgerKey = CONFIG_SETTING / STATE_ARCHIVAL)
curl -s -X POST https://mainnet.sorobanrpc.com -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLedgerEntries","params":{"keys":["AAAACAAAAAo="],"xdrFormat":"json"}}'
```

**Policies carry the plot coordinates and must be `Drought`.** `create_policy` takes
`lat_e6` and `lon_e6` (degrees x 1e6, i64) right after `plot_id`. Compute them from the
same plot record the Lit action reads and round the same way (`Math.round(deg * 1e6)`); a
determination whose signed coordinates differ by more than 1 micro-degree on either axis
reverts `PlotLocationMismatch` (321). The only methodology the signer can sign,
CROP_DROUGHT (`crop-drought-rdi-1.0`), settles only `CoverageType::Drought`, so
`create_policy` refuses any other coverage type with `CoverageNotOffered` (118);
PayoutReceiver keeps the claim-time check (`CoverageNotSettledByMethodology`, 320). The EVM
backend creates policies as COMPREHENSIVE; the Stellar backend must create them as DROUGHT
(`CoverageType` 0).

**Sell drought cover for the whole season, before planting.** A drought determination
settles only if (a) the planting day (`season_start_epoch`, 00:00 UTC) is on or after the
UTC day the cover started — `activate_policy` resets `start_date` to now — and (b) the
110-day measurement window ends by `end_date`; otherwise it reverts
`EvidenceOutsideCoverWindow` (326). It is also refused before the window ends
(`SeasonNotComplete`, 327). The oracle runs at least 7 days after the window (archive lag).
So: sell at least ~140 days of cover and activate on or before planting day. Cover bought
after planting cannot settle that season; this is the anti-adverse-selection rule, not a
bug.

**Submitting a drought determination.** The relayer submits exactly what the Lit action
signed (the oracle's `stellar` envelope: `determination` as snake_case fields, `signature`
as 64-byte `r‖s`, `recoveryId` 0/1) within 3600 s of `assessed_at`:

```bash
stellar contract invoke --id "$PR" --network testnet --source relayer \
  -- submit_drought_determination --caller <RELAYER_G...> \
  --d '<DroughtDetermination JSON>' --signature <128 hex> --recovery_id <0|1>
# what the contract will verify, for comparison with the oracle's digest:
stellar contract invoke --id "$PR" --network testnet --send no \
  -- drought_determination_digest --d '<DroughtDetermination JSON>'
```

`sum_insured` and `payout_amount` are 7-decimal USDC (a 6dp amount fails 311).

**Freeze the coordinates on the policy.** The coordinates are fixed at `create_policy`. The
backend must store the `lat_e6` / `lon_e6` it passed on its own policy record, and the Lit
action must sign those stored values, not the live plot record. If the plot is edited or
re-geocoded after sale and the signer reads the new values, every determination for that
policy reverts `PlotLocationMismatch`.

**Premiums are paid exactly once, in full.** `receive_premium` takes exactly the policy's
`premium`; any other amount reverts `PremiumAmountMismatch` (223).

**Signer rule: one determination per policy per window.** Two determinations signed for the
same policy are both valid for an hour, and whichever is submitted first settles it. The
contract does not arbitrate. The Lit action must not sign a second determination for a
policy while an earlier one is within `MAX_REPORT_AGE` (3600 s) or the policy is paid or
claim-pending.

**Expiry is permissionless.** `expire_policy(policy_id)` needs no role once
`now > end_date`; a keeper can run it to release the org's reserve and the farmer's slot.
This includes PENDING policies that were never activated (their `end_date` is creation time
plus duration); a lapsed PENDING policy can no longer be activated. A premium already paid on
it stays in the org reserve, so any refund is off-chain.

**Every farmer needs their own address.** The open-policy cap (3, counting claims in the
last 365 days) is per farmer `Address`. If the backend falls back to one shared wallet for
farmers without a wallet, as the EVM backend does with `backendWallet`, the cap applies to
the whole platform.

**Cancelling ACTIVE cover is Admin-only.** The backend key can cancel PENDING policies. An
ACTIVE policy can be cancelled only by the admin key (escape hatch); a claim-pending policy
cannot be cancelled at all.

**Reserve ratio floor.** `set_org_reserve_ratio_bps` accepts 3000–10000 (30%–100%); the
default is 3000.

**Farmer cap.** A farmer may hold at most 3 open policies, counting claims paid in the last
365 days against the same cap. Expect `TooManyActivePolicies` for a farmer with recent
claims, not only for one with many open policies.

**Paged indexes.** `get_farmer_policies` and `get_distributor_policies` take
`(start, limit)` and return at most 50 ids per call (a transaction may touch at most 100
ledger entries); use the matching `*_count` view to page.

## Upgrading to the drought path and the rent cap

For a deployment made before 2026-10-18 (testnet today). PolicyManager, Treasury and
PayoutReceiver upgrade in place and keep their storage; **PolicyNft has no `upgrade`** (as
in the Solidity), so it keeps the old extend-to-maximum code on its own entries. Order:

```bash
cargo build --target wasm32v1-none --release
for c in policy_manager treasury payout_receiver; do
  stellar contract optimize --wasm target/wasm32v1-none/release/microcrop_$c.wasm
done
# upload each optimized wasm; each prints a 32-byte hash
PM_HASH=$(stellar contract upload --wasm target/wasm32v1-none/release/microcrop_policy_manager.optimized.wasm --source admin --network testnet)
TR_HASH=$(stellar contract upload --wasm target/wasm32v1-none/release/microcrop_treasury.optimized.wasm --source admin --network testnet)
PR_HASH=$(stellar contract upload --wasm target/wasm32v1-none/release/microcrop_payout_receiver.optimized.wasm --source admin --network testnet)
# upgrade (Upgrader role)
stellar contract invoke --id "$PM" --source admin --network testnet -- upgrade --caller <ADMIN_G...> --new_wasm_hash "$PM_HASH"
stellar contract invoke --id "$TREASURY" --source admin --network testnet -- upgrade --caller <ADMIN_G...> --new_wasm_hash "$TR_HASH"
stellar contract invoke --id "$PR" --source admin --network testnet -- upgrade --caller <ADMIN_G...> --new_wasm_hash "$PR_HASH"
# enable CROP_DROUGHT; CROP_DAMAGE stays disabled (absent = disabled)
stellar contract invoke --id "$PR" --source admin --network testnet -- set_methodology_enabled --caller <ADMIN_G...> --kind 1 --enabled true
# keeper: all four to 30 days
stellar contract invoke --id "$PR" --source admin --network testnet -- extend_contract_ttl
./scripts/verify-deployment.sh --network testnet
```

Each upload is a new code entry: it starts at the network minimum and the old code entry's
prepaid rent is lost, which is why the keeper target is 30 days and not the maximum. After
the upgrade, a claim already recorded as pending still settles (`settle_pending_claim` does
not re-verify), but no new crop determination is accepted (322).

**Rent checks after the upgrade.** Simulate every entrypoint: nothing above ≈ 1.5 XLM except
the keeper; `extend_contract_ttl`'s cost should be about the days extended × 14.4 XLM on
testnet; `liveUntilLedgerSeq` of the instances and code entries ≈ now + 30 days.

**Signer.** Until the drought Lit action signs the Soroban preimage (`chain = stellar`,
`networkDomain`, the contract StrKey as `verifyingContract`) and its new CID is registered
with `scripts/rebuild-group.ts` (master key), no determination can verify. Check the live
PKP's public key equals `AUTHORIZED_SIGNER_PUBKEY` and its digest equals
`drought_determination_digest` for the deployed contract and domain.

**Live negative checks** on testnet: a historical season for a policy activated now → 326;
Comprehensive at sale → 118; a crop determination → 322. A live positive settlement on
testnet needs a real season inside a live policy (planting March–May 2027, window end
≈ July 2027); until then the positive path is proven by `real_era5_season_settles_end_to_end`.

## After a successful testnet deploy

1. Record the four addresses (and Stellar Expert links) — these are the Instaward Deliverable 1 evidence.
2. Give `NETWORK_DOMAIN` to whoever updates the Lit PKP signer. Nothing verifies until both sides match.
3. Run the cross-contract flow against live testnet and capture the transaction hash — Deliverable 3.
