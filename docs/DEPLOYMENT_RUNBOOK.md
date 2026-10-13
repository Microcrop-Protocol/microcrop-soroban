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
money-path roles are granted, and the signer and domain are set. Exits non-zero on any
failure.

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

Storage TTL is now set against the measured network limit (see **Storage TTL: keep open
policies alive** below), but the restore cost of an entry that archives anyway has not
been measured.

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

**A frozen backend wallet queues claims; rotate it.** Circle's Stellar USDC issuer has
`auth_revocable` set (checked on mainnet and testnet on 2026-10-13; `auth_clawback_enabled`
is off, so clawback is not a risk). If the backend wallet's trustline is deauthorized, or
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

**Storage TTL: keep open policies alive.** Soroban archives an entry whose TTL runs out.
The network's `max_entry_ttl` is 3,110,400 ledgers (~180 days at 5 s) on both mainnet and
testnet (measured 2026-10-13 with `getLedgerEntries` on `ConfigSettingStateArchival`;
re-check with the query below). The contracts extend every entry they touch to that
maximum, but a 365-day policy outlives one TTL, and a policy nobody touches for ~180 days
archives. The backend must call the permissionless keep-alive for every PENDING, ACTIVE or
CLAIM_PENDING policy at least every **150 days** (and once more after a claim, so the paid
flags outlive any late dispute):

```bash
stellar contract invoke --id "$PR" --network mainnet --source <any funded key> \
  -- extend_policy_ttl --policy_id <id>
```

It extends the policy's entries in all four contracts (PayoutReceiver `PolicyPaid` /
`Report` / `ClaimPending`, Treasury `PremiumReceived` / `PayoutProcessed` / `PendingPayout`
and its queue slot, PolicyManager `Policy` / `PendingCounted`, PolicyNft certificate) and
each contract instance. An entry that archives anyway is not lost: it is restored
(`stellar contract restore`, or automatically by a transaction whose footprint includes
it), at a fee. Per-farmer and per-org counters are touched by every policy of that farmer
or org; an org or farmer with no activity for ~180 days needs a restore on next use.

```bash
# re-check the network limit (read-only; LedgerKey = CONFIG_SETTING / STATE_ARCHIVAL)
curl -s -X POST https://mainnet.sorobanrpc.com -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getLedgerEntries","params":{"keys":["AAAACAAAAAo="],"xdrFormat":"json"}}'
```

**Policies carry the plot coordinates and must be `Comprehensive`.** `create_policy` now
takes `lat_e6` and `lon_e6` (degrees x 1e6, i64) right after `plot_id`. Compute them from
the same plot record the Lit action reads and round the same way (`Math.round(deg * 1e6)`);
a determination whose signed coordinates differ by more than 1 micro-degree on either axis
reverts `PlotLocationMismatch` (321). The only determination kind, CROP_DAMAGE, settles only
`CoverageType::Comprehensive` policies; any other coverage type reverts
`CoverageNotSettledByMethodology` (320) at claim time, so create every Stellar policy as
`Comprehensive` (the EVM backend already does).

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

## After a successful testnet deploy

1. Record the four addresses (and Stellar Expert links) — these are the Instaward Deliverable 1 evidence.
2. Give `NETWORK_DOMAIN` to whoever updates the Lit PKP signer. Nothing verifies until both sides match.
3. Run the cross-contract flow against live testnet and capture the transaction hash — Deliverable 3.
