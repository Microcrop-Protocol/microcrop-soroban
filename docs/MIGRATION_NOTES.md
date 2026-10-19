# MicroCrop: EVM → Soroban Migration Notes

**Honest framing:** this workspace is a **preparation port**, not a completed migration.
The four contracts are faithfully re-implemented in Rust, compile to `wasm32v1-none`
WASM, and pass their unit and integration suites (200 tests, plus 1 ignored real-WASM
lane that passes after a wasm build). Treasury-era testnet deployments exist (2026-10,
`deploy-v2`); Option B has not been deployed yet. The contracts are not integrated with the
off-chain stack and have no mainnet deployment. An AI-assisted Soroban review (2026-10) found 11
issues; all are fixed on this code (see
[§6](#6-audit-fixes-2026-10--where-the-port-now-departs-from-the-solidity)), but that review
is not a substitute for an independent audit.

**2026-10-18:** PayoutReceiver settles CROP_DROUGHT determinations (`crop-drought-rdi-1.0`,
the live methodology) and storage rent is capped. See [§9](#9-crop_drought-path-option-a)
and [§10](#10-rent-cap).

**2026-10-19 — Option B:** the custodial per-org Treasury is removed. A verified
determination becomes an obligation in a non-custodial `SettlementAdapter`
(`Owed → Funded → Settled`, attested off-chain payment), the PayoutReceiver's adapter is
admin-settable behind a binding handshake, and no contract holds a token. See
[§11](#11-option-b-settlementadapter).
The [DEFERRED](#deferred--not-yet-done) section is the load-bearing part of this document.

**Product-architecture decision (2026-08-21; first step shipped as Option B, §11):** the
faithful Treasury port is now a legacy/reference implementation, not the target mainnet
design (it was removed from this workspace on 2026-10-19 and lives in git history at
`ed52203`). The carrier will keep
its statutory reserve and investments off-platform. Soroban should record a
local-currency policy and verified payout obligation, while a bounded funding facility
and regulated settlement adapter execute the last-mile payment. The obligation should
also commit to a CID/content hash for an encrypted evidence bundle stored through Logos
Storage. Preserve the existing port for parity tests, but do not wire it into a live
carrier pilot as mandatory reserve custody.

Source of truth for the port: `microcrop-contracts/microcrop/src/*.sol` (Solidity 0.8.28,
PolicyManager/Treasury v3.1.0, PayoutReceiver v2.1.0).

- Verified toolchain: **soroban-sdk `=27.0.2`** (crates.io, published 2026-07-23),
  Rust **1.91.0** MSRV, target **`wasm32v1-none`**.
- Docs cited: <https://developers.stellar.org/docs/build/smart-contracts>,
  soroban-sdk API (<https://docs.rs/soroban-sdk/27.0.2/>), SEP-41 token interface / SAC,
  `env.crypto()` (secp256k1_recover / keccak256), storage TTL, `#[contracterror]`,
  `#[contractevent]`, `Address::require_auth`, testutils (`Env::default`, `mock_all_auths`).

---

## 1. Platform mapping (what a Solidity concept becomes)

| Solidity / EVM                                   | Soroban / soroban-sdk 27.0.2                                                                 |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------- |
| `contract`, external/public functions            | `#[contract] struct` + `#[contractimpl]` inherent methods                                    |
| `constructor`                                     | `__constructor(env: Env, ...)` (SDK constructor convention)                                  |
| `msg.sender` + `onlyRole`/`require`               | explicit `caller: Address` param + `caller.require_auth()` + `roles::require_role`           |
| OpenZeppelin `AccessControlUpgradeable`          | `crates/shared/roles.rs` — `RoleKey::Role(Role, Address)` persistent registry                |
| `struct` / `enum`                                | `#[contracttype]` structs & enums (in `crates/shared/types.rs`)                              |
| `error MyError()` / custom errors                | `#[contracterror] #[repr(u32)]` enums (`crates/shared/errors.rs`) + `panic_with_error!`      |
| `event X(...); emit X(...)`                       | `#[contractevent]` structs with `#[topic]` (= Solidity `indexed`), `.publish(&env)`         |
| `mapping(k => v)`                                 | typed `DataKey(k)` storage entries (`storage().persistent()/instance()/temporary()`)         |
| storage (permanent, gas-paid once)               | `instance` (small config, shares contract TTL) / `persistent` (per-entity, own TTL) / `temporary` (short-lived replay markers) — all with explicit `extend_ttl` |
| `block.timestamp`                                | `env.ledger().timestamp() -> u64`                                                            |
| `block.chainid` (replay domain separation)       | **`network_domain: BytesN<32>`** stored + admin-settable (see §4)                            |
| `address(this)`                                  | `env.current_contract_address()`                                                             |
| `abi.encode` / `keccak256` / `ecrecover`         | `shared::encoding` byte layout + `env.crypto().keccak256` + `env.crypto().secp256k1_recover` |
| ERC-20 USDC (`transfer`, allowances)             | none since Option B (§11): no contract holds or moves a token; amounts are 7dp USDC base units as a unit of account. (The Treasury used the SEP-41 Stellar Asset Contract via `TokenClient`.) |
| UUPS `upgradeToAndCall`                          | `env.deployer().update_current_contract_wasm(hash)` behind an `Upgrader`-role `upgrade()` fn |
| `try/catch` on external call                     | generated client's `try_*` method returning `Result`                                        |
| cross-contract call (interface)                  | `#[contractclient]` trait → generated `...Client` (`shared/interfaces.rs`)                   |
| revert on overflow                               | `overflow-checks = true` in the release profile (money i128 traps, never wraps)             |

---

## 2. Per-contract mapping & what is PRESERVED

### PolicyManager (`crates/policy-manager`) ← PolicyManager.sol v3.1.0
Preserved: duration bounds; PENDING→ACTIVE window reset preserving duration; per-org
**outstanding exposure** accounting (it drove Treasury solvency; since Option B it is a view
only); id starts at 1;
`PendingCounted` flag (Finding 8). NFT status updates are **best-effort** via the fallible
`try_update_policy_status` client (Soroban equivalent of Solidity `try/catch`) so lifecycle
transitions can never be bricked. Changed by the 2026-10 audit fixes (§6): sum bounds in
7dp; exposure accrues at **create** (release on cancel/expire/claim); open cap is
`open (ACTIVE+PENDING) + claims in the rolling window ≤ 3`; claim cap is 3 per rolling
365 days (not a calendar-year bucket); farmer index is paginated; a `ClaimPending`
status (removed by Option B, §11: ordinal 5 is retired). Changed by §7: `create_policy` takes the plot coordinates (`lat_e6`, `lon_e6`,
degrees x 1e6) and stores them on `Policy`; permissionless `extend_policy_ttl(policy_id)`. Roles: create/activate = `Backend`; cancel PENDING = `Backend` or `Admin`, cancel
ACTIVE = `Admin` only; expire = anyone after `end_date`;
mark_as_claimed/increment_claim_count = `Oracle` (`mark_claim_pending` removed by Option
B; `mark_as_claimed` accepts ACTIVE only); setters = `Admin`; upgrade = `Upgrader`.

### Treasury — removed by Option B (2026-10-19)
The crate, its tests and snapshots were deleted; the history below describes the port as it
was at `ed52203` and applies only to Treasury-era deployments. Its error range 200..=299
(215/216 included) is retired and never reused. See §11.

### SettlementAdapter (`crates/settlement-adapter`) — new, no Solidity source
Non-custodial obligation ledger (§11). `record_obligation` only by the bound PayoutReceiver
(address check + its auth); `mark_funded` / `mark_settled` by the obligation's stored org or
`Role::Funder` (7); `set_payout_receiver`, `grant_role`, `revoke_role` = `Admin`; `upgrade`
= `Upgrader`. Holds no tokens, makes no cross-contract calls, no pause. Errors 500..=507.

<details><summary>Treasury port as it was (Treasury-era deployments only)</summary>

Money math preserved 1:1: premium split `fee = amount * fee_bps(org) / 10_000`;
per-org reserve credit; `receive_premium` pulls USDC from caller (`TokenClient::transfer`);
`request_payout` funds **only** from the policy's org reserve; surplus-only withdrawals
(`WouldBreachReserve`); `emergency_withdraw` touches only *unbacked* surplus
(`balance − Σ reserves − fees`). Changed by §6: a short reserve records a **pending
payout** instead of reverting (`settle_pending_payout` pays it later);
`reserve_required = outstanding × ratio + pending payouts`; reserve ratio default and
floor 30%; `receive_premium` only for PENDING, or ACTIVE-and-not-ended, policies; the
constructor refuses a token whose `decimals() != 7`. Changed by §7: `receive_premium`
requires `amount == policy.premium`; pending payouts form a per-org first-in-first-out
queue and only the head settles; a failed USDC transfer to the backend wallet records the
claim as pending instead of reverting (`try_transfer`), so the transfer now runs before
the payout bookkeeping (safe: Soroban has no re-entrancy, and nothing is written when it
fails). CEI ordering (effects before the USDC interaction)
kept. Solvency invariant `balance ≥ total_org_reserves + accumulated_fees` asserted in tests.
Roles: `receive_premium` = `Backend`; `request_payout` = `Payout` **and** `caller ==
payout_receiver`; reserve/fee setters = `Admin`.

</details>

### PayoutReceiver (`crates/payout-receiver`) ← PayoutReceiver.sol v2.1.0 (security crux)
The full **12-step validation runs in the exact Solidity order**, then CEI effects, then
interactions. Signature is the **sole** payout authority; `Relayer` is anti-spam only.
Preserved: bounds/consistency checks; weighted invariant with **no division** (`60w+40s==bp`,
satellite-only `100s==bp`); threshold 3000; `sum_insured` match; **payout re-derived
on-chain** and compared to the signed value; freshness (`MAX_REPORT_AGE=3600`, saturating);
farmer claim limit; on-chain digest reconstruction via `shared::encoding::
determination_digest`; `secp256k1_recover` byte-compared to the stored SEC-1 65-byte key;
**triple replay guard** (temporary `ConsumedDetermination` + persistent `PolicyPaid` +
Treasury `PayoutProcessed`; since Option B the third layer is the adapter's one obligation
per (PayoutReceiver, policy)). **Option B (§11):** the Treasury call is replaced by
`SettlementAdapter.record_obligation` with a field-by-field return check (329); the adapter
is admin-settable (`set_settlement_adapter`, bound by handshake, 328); `settle_pending_claim`,
`is_claim_pending` and the pending-claim machinery below are removed.
Added by §6: when Treasury reports a short reserve the policy
becomes `ClaimPending` and the permissionless `settle_pending_claim(policy_id)` pays it in
full later. Added by §7: step 6c rejects any coverage type but `Comprehensive`, step 6d
binds the signed coordinates to the policy's plot, `settle_pending_claim` accepts only the
org's queue head, and the permissionless `extend_policy_ttl(policy_id)` bumps the policy's
entries in all four contracts. The contract passes `env.current_contract_address()` as the
`caller` to its PolicyManager and SettlementAdapter (formerly Treasury) sub-invocations (a contract auto-authorizes its own
sub-calls).

### PolicyNFT (`crates/policy-nft`) ← PolicyNFT.sol
`tokenId == policyId`; `policy_id == 0` rejected (reserved sentinel); re-mint rejected;
**soulbound while active** (`transfer` reverts `TransferWhileActive` if `cert.is_active`);
`update_policy_status` callable **only** by the stored PolicyManager address (Finding 6);
distributor index preserved but stored one entry per token and read in pages
(`get_distributor_policies(distributor, start, limit)`, §6). Minter role gates `mint_policy`.

---

## 3. What NECESSARILY CHANGED (platform-forced, not design changes)

1. **Zero-address guards dropped.** A Soroban `Address` is always a valid host object; there
   is no `address(0)`. All `ZeroAddress*` checks are structurally unreachable. Error *codes*
   are kept in the shared enums for parity, but nothing checks them. Consequence:
   `policy_org(unknown)` **panics `PolicyDoesNotExist`** instead of returning `address(0)`;
   `get_certificate`/`owner_of` on an unknown token **revert `TokenNotFound`** rather than a
   zeroed struct. Callers must not rely on a zero-address sentinel.
2. **`block.chainid` → `network_domain: BytesN<32>`.** Soroban has no `chainid`. Domain
   separation for the determination digest is a stored 32-byte value set by
   `set_network_domain` (Admin, port-only new setter). **The off-chain Lit PKP signer MUST
   use the identical 32 bytes**, or every signature fails recovery.
3. **USDC = SEP-41 Stellar Asset Contract**, not ERC-20 (Treasury era; since Option B no
   contract holds a token, but every amount is still 7dp USDC base units). Balances via `TokenClient`; the
   contract holds SAC balances directly (no trustline provisioning in-contract). **Stellar
   USDC has 7 decimals, not 6**: 1 USDC = 10_000_000 base units. Every amount constant is
   restated in 7dp (`MIN_SUM_INSURED = 1_000_000_000` = 100 USDC, `MAX_SUM_INSURED =
   10_000_000_000_000` = 1,000,000 USDC) and `Treasury.__constructor` panics
   `UsdcDecimalsMismatch` (217) unless the token reports `decimals() == 7`. The Lit signer
   must scale amounts by 1e7 on Stellar and keep 1e6 on Base.
4. **On-chain SVG/JSON `tokenURI` dropped** from PolicyNFT. Rendered off-chain from the
   stored `PolicyCertificate`. `baseExternalURI` also dropped.
5. **PolicyNFT is not upgradeable.** The Solidity `PolicyNFT` is plain AccessControl (no
   UUPS), so there is no `upgrade` fn — matching source. The other three keep `upgrade`.
6. **Explicit `caller` + `require_auth`** everywhere replaces implicit `msg.sender`.
7. **Storage TTL / rent.** No EVM equivalent: an entry whose TTL lapses is archived and
   must be restored before use, and every extension is charged up front. Measured
   2026-10-13 (`getLedgerEntries` on `ConfigSettingStateArchival`, protocol 29):
   `max_entry_ttl` = **3,110,400 ledgers (~180 days at 5 s)** on both mainnet and testnet;
   `min_persistent_ttl` is 2,073,600 (mainnet) / 120,960 (testnet). Since 2026-10-18 the
   contracts follow a **rent cap** (§10): ordinary calls extend instance + code only below
   3 days, to 10 days; a permissionless keeper (`extend_contract_ttl`) extends all four
   contracts to 30 days; data entries live as long as the policy needs them
   (`end_date + 30 d`, 60 days for open obligations and role grants). The temporary replay
   marker stays at ~7 days (it only has to outlive the 1-hour freshness window).
8. **Events use `#[contractevent]`.** `env.events().publish(...)` is deprecated in SDK 27;
   `#[topic]` fields replace Solidity `indexed`. Some zero-value "previous value" event
   fields were dropped (no zero Address to emit).
9. **Pausable error codes.** OpenZeppelin `Pausable` has no code in the frozen shared
   `TreasuryError`/`PayoutError` ranges, so Treasury and PayoutReceiver each define a small
   crate-local `#[contracterror]` paused code (`215/216` and `318` respectively). See §5.
   215/216 are retired with the Treasury; the SettlementAdapter has no pause (§11).
10. **Legacy shims omitted.** `setLegacyPolicyOrg`, `__gap`, `__deprecated` storage padding
    are gone — this is a fresh chain with no state to migrate or storage layout to preserve.

---

## 4. Frozen cross-contract contract (do not drift)

`crates/shared` is the coordination boundary; changing it requires re-checking all four
contracts. The cross-contract client traits (`interfaces.rs`) since Option B:

```rust
// PayoutReceiver -> SettlementAdapter
pub trait ISettlementAdapter {
    fn record_obligation(env: Env, caller: Address, policy_id: u64, org: Address,
                         amount: i128, determination_digest: BytesN<32>) -> Obligation;
    fn payout_receiver(env: Env) -> Option<Address>;
    fn find_obligation(env: Env, payout_receiver: Address, policy_id: u64) -> Option<u64>;
    fn extend_obligation_ttl(env: Env, obligation_id: u64);
}
// PayoutReceiver -> PolicyManager (mark_claim_pending, org_outstanding_sum_insured and
// policy_org are no longer called cross-contract; the views stay on PolicyManager)
pub trait IPolicyManager {
    fn get_policy(env: Env, policy_id: u64) -> Policy;
    fn policy_exists(env: Env, policy_id: u64) -> bool;
    fn can_farmer_claim(env: Env, farmer: Address) -> bool;
    fn mark_as_claimed(env: Env, caller: Address, policy_id: u64);
    fn increment_claim_count(env: Env, caller: Address, farmer: Address);
    fn extend_policy_ttl(env: Env, policy_id: u64);
    fn extend_contract_ttl(env: Env);
}
```

`ITreasury` is deleted. `crates/shared/src/abi_tests.rs` pins every error code number and
the `Role`, `PolicyStatus` and `ObligationStatus` ordinals. In particular **`encoding.rs` is FROZEN**: both determination byte layouts must
byte-for-byte match the off-chain signer.

| | CROP_DAMAGE (retired) | CROP_DROUGHT |
|---|---|---|
| methodology | `crop-dualindex-1.0` | `crop-drought-rdi-1.0` |
| kind word | `keccak("CROP_DAMAGE")` = `0x5f9a…` | `keccak("CROP_DROUGHT")` = `0xf2b99be7…b6fe` |
| evidence | 10 words (`encode_inputs`) | 13 words, 416 B (`encode_drought_inputs`) |
| result | 6 words | 5 words: policy id, bp, drought damage, payout, assessed_at |
| preimage | 392 B | 360 B |

Common to both: 32-byte big-endian two's-complement words, `keccak("1.0")` schema word,
`network_domain` (`sha256(passphrase)`) in place of `block.chainid`, the 40-byte contract
XDR (`00000012 00000001 ‖ id`) in place of `address(this)`, digest = `keccak(preimage)` is
both the recovery message and the replay key. Golden vectors: `crates/shared/src/encoding/
tests.rs` (`drought_golden_vectors`). `interfaces.rs` client signatures must match the real
inherent methods.

---

## 5. DEFERRED / NOT-YET-DONE (be honest)

These are known gaps. None are "bugs" in the ported logic; they are the work that turns a
preparation build into a real migration.

**Integration & deployment**
- [x] ~~**Not deployed** anywhere.~~ Testnet deployment exists (2026-10,
      `deployments/testnet.json`). Mainnet: not deployed.
- [ ] **Option B is not on testnet yet.** It cannot be reached by upgrading a Treasury-era
      deployment: deploy fresh (`deploy.sh --state-dir`), verify, then run
      `scripts/testnet-evidence.sh` (§11, runbook *Testnet evidence*).
- [x] ~~**No end-to-end / cross-contract integration test.**~~ DONE — `crates/integration-tests`
      registers all four REAL contracts on one `Env` (no mocks, no token) and covers
      sale → determination → obligation (Owed → Funded → Settled), replay (also after an
      adapter swap), the unbound adapter, wrong-signer rejection and the keepers; the
      `#[ignore]`d `wasm_lane_full_flow` repeats the core flow against the optimized WASM
      from `target/`. Still HOST-SIDE only: running it against live testnet, with real
      transaction hashes, is `scripts/testnet-evidence.sh` (not run yet).
- [x] ~~deploy script / role-granting runbook not automated~~ DONE — `scripts/deploy.sh`
      (idempotent, resumable, dry-run, mainnet-gated) and `scripts/verify-deployment.sh`
      (exact role checks via the new `has_role` view) plus `docs/DEPLOYMENT_RUNBOOK.md`.
- [x] ~~**USDC SAC address**~~ no longer needed: no contract touches a token (Option B).
- [x] ~~`network_domain` value not chosen~~ DECIDED — derived as `sha256(network passphrase)`:
      testnet `cee0302d…ecd472`, mainnet `7ac33997…45a979`. It does what `block.chainid` did
      (binds a determination to one network) and both sides compute it from a public constant,
      so no magic number has to be shipped. Set by `scripts/deploy.sh`; overridable.
- [ ] **The Lit drought action does not yet sign the Soroban drought preimage** (spec Part 3,
      `microcrop-lit-oracle`: `src/chain/stellar-drought.shared.js`, `chain`/`networkDomain`
      jsParams in `lit-actions/drought-oracle.js`, `drought.ts`, `drought-scope.ts`). Until it
      ships and its new CID is re-registered (`scripts/rebuild-group.ts`, master key), the
      production PKP cannot sign anything this PayoutReceiver accepts. The JS encoder must
      reproduce `drought_golden_vectors` byte for byte. Until then the drought orchestrator
      must skip policies with `chain === 'stellar'`: it passes the Base domain, so a Stellar
      policy would get a valid Base signature for the Base policy with the same numeric id.
      Note the contract stores the PKP's 65-byte SEC-1 PUBLIC KEY, not its 0x address.
- [ ] **Backend / indexer / relayer integration** untouched — the EVM stack (ethers.js
      listeners, Ponder indexer, relayer) still targets Base. Ledger events, XDR decoding,
      and Stellar RPC are not wired.

- [ ] **Each methodology is a frozen constant in its own preimage.** The drought path is a
      second entrypoint with its own layout (§9); which methodologies are accepted is an admin
      switch (`MethodologyKind`), but **adding one is still a contract upgrade plus an audit**
      (by design: the methodology string is signed, not a parameter). rdi-1.1 (basis-point
      native, debiased) would be a third `MethodologyKind` with its own step 4/5 invariant.
- [ ] **There is no observation-source field anywhere in the signed preimage** — no dataset, no
      product version, no served grid cell. The determination-stability work
      (microcrop-context/docs/DETERMINATION_STABILITY.md) found the same farmer gets a different
      payout band on 39% of seasons under RDI, and 86% under CDD, purely from the choice of
      rainfall product. The leading remedy is to declare a canonical source and SIGN it — which
      the current on-chain schema cannot express at all. The off-chain canonical determination
      can (it carries `methodologyVersion` and `provenance.url`); the on-chain verifier cannot.

**Verification & safety**
- [ ] **No Soroban security audit.** The Solidity was audited; this Rust port has not been.
      The secp256k1 signature path, replay guards, the PayoutReceiver ↔ adapter binding and
      the obligation state machine need fresh review on Soroban semantics (auth trees,
      TTL/archival edge cases). Skill-checklist flags are listed in §11.
- [x] ~~**TTL/rent policy is placeholder.**~~ Rent cap (§10). An archived persistent entry
      fails the read rather than reading as absent, so archival does not open a replay; the
      cost is a restore (`an_archived_policy_paid_flag_never_pays_out`).
- [ ] **Keeper job not built.** The backend must call `PayoutReceiver.extend_contract_ttl()`
      daily and `extend_policy_ttl(id)` for open policies and policies with an Owed or Funded
      obligation whose shortest TTL is under 30 days (runbook), and must submit them even though simulation reports them
      read-only (CLI: `--send=yes`). Without it, code and instances fall back to the 10-day
      user-path safety net and the first call after a lapse pays up to ~10 days of code rent
      for each contract it touches (§10). Alert on keeper failure.
- [ ] **Code size grew** (optimized wasm, `stellar contract optimize`, 2026-10-18; code rent
      scales with it): PayoutReceiver 45,271 -> 59,816 B (the drought path), PolicyManager
      47,369 -> 52,602, Treasury 60,283 -> 64,683, PolicyNft 29,246 -> 38,481. Roughly half of
      each file is the contract spec section (doc comments of every shared type and error are
      embedded in all four contracts); PolicyNft's code section grew ~4.4 KB from the need-based
      TTL helpers, not investigated further. Trimming spec docs and the helpers are rent levers.
      Option B (2026-10-19, optimized): PayoutReceiver 62,527 B, PolicyManager 51,758,
      SettlementAdapter 41,033 (the Treasury's 64,683 is gone), PolicyNft 39,725.
- [ ] **Shared paused-code fragmentation** (§3.9): recommend adding one shared paused error
      to `microcrop_shared::errors` so all contracts share a code, instead of three local
      enums. Deferred to avoid touching the frozen shared crate mid-port.
- [ ] **`org` resolution panic semantics** (§3.1): `policy_org(unknown)` panics rather than
      returning a sentinel; confirm no Soroban caller depends on the old address(0) behavior.
- [ ] **Fuzz / property tests** for the payout math and the encoder byte layout are not
      written (the encoder is the single most replay-sensitive surface). The adapter's state
      machine has a proptest (status monotone, one obligation per (PR, policy), settled
      reference immutable, count = distinct records).
- [x] ~~`cargo clippy` was not run~~ — now run as a local gate. 7 warnings pre-dated the
      audit fixes (duplicated test attributes, two needless borrows, one manual range
      check); Option B removed three of them with the deleted Treasury tests and added none
      (4 remain).
      `cargo fmt --all -- --check` is clean.
- [ ] **Lit signer amount scaling (drought action).** The drought action must sign
      `sum_insured` and `payout_amount` as exact 7dp Stellar USDC (`toChainBaseUnits`, not
      `Math.round(x * 1e6)` in `drought-scope.ts`). A 6dp amount fails `SumInsuredMismatch`
      (311) on every policy.
- [x] ~~**TTL against real network limits.**~~ Measured (180 days on both networks) and
      handled by extend-to-max plus `extend_policy_ttl` (§7). Still open: measure the
      restore cost of an archived entry inside the relayer's 3600 s freshness window if the
      keep-alive is missed.
- [x] ~~**Order among pending claims.**~~ Per-org first-in-first-out queue (§7); moot since
      Option B removed pending claims.
- [ ] **Signer rule: one determination per policy per assessment window.** Operational
      requirement on the Lit action, not enforced on-chain (§7, item 7).
- [ ] **Backend plot coordinates.** The Stellar backend must pass `lat_e6`/`lon_e6` to
      `create_policy` computed exactly as the Lit action does (`Math.round(deg * 1e6)` from
      the same plot record), store those values on its own policy record, and create
      policies as `Drought`. The Lit action must sign the coordinates stored for the
      policy, not the live plot record: a plot edited or re-geocoded after sale would
      otherwise make every determination revert `PlotLocationMismatch`.
- [ ] **Backend prerequisites for the drought path.** `/api/internal/active-policies`
      (microcrop-core `84fc572`) emits no `coverageType`, `plantingDate`, `seasonLengthDays`,
      `marketCode`, `chain` or `currency`, and `sumInsured` is a float, so the drought
      orchestrator skips every policy today ("coverageType absent"). For Stellar it also needs
      the Soroban u64 policy id as `onChainPolicyId` and an exact 7dp sum insured (`units.js`).
      The backend should check that the Stellar determination's `damage_percent_bp` equals the
      canonical body's `result.damagePercentBp` from the same run.
- [ ] **Sale-side rule for drought cover.** A drought determination settles only if the
      planting day is on or after the UTC day cover started and the 110-day window ends by
      `end_date` (326). Sell at least ~140 days (110 + 7-day archive lag + margin) and
      activate on or before planting day (`create_policy` enforces only the hard floor:
      Drought cover under 117 days reverts `InvalidDuration`); cover bought after planting cannot settle that
      season (intended: a grace period would leak W1 rainfall to the buyer).

**Product parity not ported**
- [ ] **Non-custodial target contracts** — first step DONE (Option B, §11): the Treasury is
      replaced by a minimal `SettlementAdapter` stub. Still open: `PolicyRegistry`, a real
      `FundingFacility` (swappable in via `set_settlement_adapter`), local-currency amounts.
      Policy and benefit amounts must be denominated in the market's local currency;
      any USDC amount belongs to a short-lived provider quote/settlement record.
- [ ] **Logos Storage manifest** — add CID, content hash, schema version, encryption-key
      reference, and retention class to the evidence commitment used by the obligation.
- [ ] On-chain `tokenURI` rendering (intentionally off-chain now) — needs an off-chain
      renderer before any NFT marketplace/explorer integration.
- [ ] Legacy migration shims (`setLegacyPolicyOrg`, storage `__gap`) — intentionally omitted;
      irrelevant on a fresh chain but noted so no one expects state import from Base.

---

## 6. Audit fixes (2026-10) — where the port now departs from the Solidity

| # | Finding | Fix | Behaviour change for callers |
|---|---|---|---|
| 1 | `PolicyNft` distributor list grew one entry until activation failed (~5,450 ids) | one persistent entry per token + a count | `get_distributor_policies(distributor, start, limit)`, `get_distributor_policy_count`; pages capped at 50 |
| 10 | `PolicyManager` farmer list, same problem on `create_policy` | same pattern | `get_farmer_policies(farmer, start, limit)`, `get_farmer_policy_count`; pages capped at 50 |
| 2 | reserve floor 20% < smallest payout 30% | `DEFAULT_RESERVE_RATIO_BPS = MIN_RESERVE_RATIO_BPS = 3_000` | `set_org_reserve_ratio_bps` below 3000 reverts `ReserveRatioTooLow` (218) |
| 3 | 5 open policies against 3 claims; fixed-year bucket let 6 claims pay | rolling 365-day window over the last 3 claim timestamps; `MAX_ACTIVE_POLICIES_PER_FARMER = 3` and `create_policy` counts open policies **plus** claims in the window | **product rule**: a farmer with 3 claims in the last 365 days cannot buy new cover until the oldest ages out. `get_farmer_claim_count(farmer)` (no year), `get_farmer_claim_times`; `get_current_year` removed |
| 4 | a PENDING policy's premium was withdrawable surplus | exposure counted at `create_policy`, released on every exit exactly once | `org_outstanding_sum_insured` includes PENDING policies |
| 5 | amounts in 6dp, Stellar USDC is 7dp | constants restated; constructor checks `decimals() == 7` | see §3.3; signer follow-up in §5 |
| 6 | Backend could cancel ACTIVE cover after a loss | Backend cancels PENDING only; ACTIVE needs Admin | `CancelActiveRequiresAdmin` (116) |
| 7 | only Backend could expire an ended policy | permissionless; `now > end_date` is the gate; repeat call is a no-op | `expire_policy(policy_id)` (no caller); event `PolicyExpired` |
| 8 | premium credited to a closed policy | read the policy; PENDING, or ACTIVE and not ended | `PolicyNotOpenForPremium` (219), `PolicyCoverageEnded` (220) |
| 9, 11 | (superseded by Option B, §11: pending claims removed) a short reserve reverted a valid claim with no record; it then lapsed | **pending claims**: Treasury records the full payout, the policy becomes `ClaimPending` (cannot expire, be cancelled, or be settled by another determination), `reserve_required` includes it, and anyone can call `PayoutReceiver.settle_pending_claim(policy_id)` once the reserve covers it | `request_payout -> bool`; new `settle_pending_payout`, `get_pending_payout`, `org_pending_payouts`, `mark_claim_pending`, `settle_pending_claim`, `is_claim_pending`; `PolicyStatus::ClaimPending = 5`; errors 221, 222, 319 |

Leads fixed alongside: role grants extend their TTL on grant and on use; persistent
entries in all four contracts request ~400 days (was 30 / 90), and paged reads extend the
index entries they read.

Review follow-ups in the same PR:
- A PENDING policy past its `end_date` (creation + duration) can be expired by anyone,
  which releases its org exposure and pending slot; `activate_policy` refuses a lapsed
  PENDING policy (`PolicyExpired`, 111).
- `request_payout` pays at once only from `org_reserve − org_pending_payouts`, so a new
  claim cannot take money held for an older pending claim.
- `mark_claim_pending` releases the active slot and the org exposure (the claim already
  counts as a claim in the window and as a pending payout), so a ClaimPending policy is
  not counted twice; `mark_as_claimed` from ClaimPending releases nothing again.
- `settle_pending_payout` reverts `NotConfigured` (3) unless the PayoutReceiver is wired.
- `scripts/deploy.sh` checks `decimals() == 7` on `USDC_SAC` before deploying anything.

## 7. Review follow-ups (2026-10-13)

| # | Lead | Fix | Behaviour change for callers |
|---|---|---|---|
| 1 | pending claims settled in any order; whoever called first chose which claim a partial top-up paid | Treasury keeps a per-org queue (`OrgQueueHead`, `OrgQueueTail`, one `OrgQueueAt(org, i)` entry per claim; no `Vec`). `PendingPayout.queue_index` records the slot; `settle_pending_payout` accepts only the head. Still permissionless and idempotent (a settled claim reverts `NoPendingPayout` / `NoPendingClaim`). New claims still wait behind pending ones | `settle_pending_claim(non-head)` reverts `NotPendingQueueHead` (224). New views `pending_payout_head(org) -> Option<u64>`, `pending_payout_count(org)` |
| 2 | every payout goes to one backend wallet the USDC issuer can freeze | `pay_out` uses `try_transfer`; on failure nothing is written, `PayoutTransferFailed` is emitted, and `request_payout` records the claim as pending (same queue) and returns `false`, so the determination is consumed and the policy goes `ClaimPending` instead of reverting. Settling into a still-frozen wallet reverts `PayoutTransferFailed` (225) and keeps the claim; after `set_backend_wallet` it settles | a claim can be pending while the reserve covers it. Checked 2026-10-13: Circle's Stellar USDC issuers `GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN` (mainnet) and `GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5` (testnet) both have `auth_revocable = true`, `auth_clawback_enabled = false`. The mainnet address is not the testnet USDC issuer |
| — | issuer clawback leaves reserves unbacked | **no code**: clawback is not enabled on the USDC issuer. A SAC balance (the Treasury's included) takes its clawback flag from the issuer when the balance is created, so a balance created while the flag is off stays non-clawbackable even if the issuer later enables it. Re-check the issuer flags before any redeploy | none |
| 3 | TTL requests (~400 d) were clamped to a network maximum shorter than a policy | every touch extends to `max_ttl()`; reads on the Treasury money path extend too; PolicyNft instance no longer ~7 d; new permissionless `extend_policy_ttl(policy_id)` on PayoutReceiver (fans out to Treasury, PolicyManager, and best effort PolicyNft), each also callable directly | see §3.7 and the runbook keep-alive |
| 4 | a crop-damage determination settled a policy of any coverage type | `create_policy` accepts only `Comprehensive` (`CoverageNotOffered`, 118), so single-peril cover is never sold and no premium, exposure or farmer slot is taken for cover that could not pay; step 6c keeps the same check at claim time as defence in depth | `create_policy` with any other coverage type reverts `CoverageNotOffered` (118); step 6c `CoverageNotSettledByMethodology` (320) is now unreachable for policies created by this PolicyManager. Mapping: CROP_DAMAGE (`crop-dualindex-1.0`) measures loss (60% weather, 40% NDVI), not its cause, so it can settle only all-cause cover. `Drought`, `Flood`, `Both`, `ExcessRain` name a peril the index cannot attribute. The EVM backend already writes every on-chain policy as `COMPREHENSIVE` (4) |
| 5 | signed coordinates not bound to the insured plot | `Policy.lat_e6` / `lon_e6` (i64, degrees x 1e6) set by Backend at `create_policy` (range-checked: `InvalidCoordinates`, 117); step 6d requires `|signed − stored| <= 1` per axis. One micro-degree (~0.11 m) absorbs a rounding-mode difference between the backend's and the signer's conversion of the same plot record and is far below any plot size | **ABI**: `create_policy(caller, farmer, plot_id, lat_e6, lon_e6, sum_insured, premium, duration_days, coverage_type, org)`; `Policy` gains two fields; `PlotLocationMismatch` (321) |
| 6 | `receive_premium` accepted any amount | `amount == policy.premium` | `PremiumAmountMismatch` (223) |
| 8 | (round 2) switching the PayoutReceiver while claims are pending strands them and, under FIFO, blocks the org's whole queue | Treasury keeps a global `PendingPayoutTotalCount` (instance); `set_payout_receiver` reverts while it is above zero | `set_payout_receiver` may revert `PayoutsStillPending` (226); new view `total_pending_payout_count()`. Settle first or upgrade the PayoutReceiver in place |
| 9 | (round 2) some long-lived org and farmer entries were never extended | Treasury reads `OrgFeeSet` / `OrgFeeBps` / `OrgRatioSet` / `OrgReserveRatioBps` through the extending getter; `extend_policy_ttl` also extends the org reserve, fee and ratio entries, the org queue head / tail / pending total while a claim is pending (Treasury), and `OrgOutstanding` plus the farmer's claim times and counters (PolicyManager) | none |
| 7 | the relayer chooses which of two signed determinations settles a policy | **signer-side rule, no contract change**: the Lit action issues at most one determination per policy per assessment window: no new signature for a policy while an earlier one is within `MAX_REPORT_AGE` (3600 s) or the policy is paid. No clean contract-side rule exists: "highest `assessed_at` consumed" cannot help because the first valid submission already pays, and any rule that lets a later determination win needs a dispute window, which delays every payout. The preimage is unchanged | none on-chain; operational requirement on the signer |

Rows 3, 4 and 9 above are superseded (2026-10-18): TTLs follow the rent cap (§10), not the
network maximum, and only `Drought` cover is sold (§9). Rows 1, 2, 6 and 8 and the Treasury
half of 9 are superseded by Option B (§11): there is no Treasury, no premium on-chain and no
pending-claim queue.

## 8. Bottom line

The Rust/Soroban re-implementation is **complete and green** as a parity/reference
version: it builds to WASM on the correct target and its tests preserve the old model's
verification invariants. Since Option B (§11) there is no custody: verified claims become
attested obligations. Still before any live pilot or mainnet consideration: a real
FundingFacility, the local-currency model, the Logos Storage commitment, the Lit PKP signer
agreement, mainnet governance (D-B3) and an independent audit.

## 9. CROP_DROUGHT path (Option A)

**Why.** The production PKP signs only `crop-drought-rdi-1.0` (the crop-oracle action was
revoked from the Lit group), so the Soroban contracts could settle nothing. The drought
action already signs a chain-neutral canonical body (SHA-256 over canonical JSON). Using
that as the payout authority (Option B) is unsound against the real code: its
`subject.policyId` is the backend's database id, not the Soroban u64 policy id; it carries
no USDC amount (`assertChainAndCurrencyNeutral` forbids `*usdc` keys); and it names no
network or contract, while testnet uses the production PKP key and canonical determinations
are published. So the drought action signs a second, chain-bound preimage (Option A), and
the canonical body stays for the notary and the partner `amountOwed`.

**Type and digest.** `DroughtDetermination` (types.rs, 18 fields) and
`encode_drought_inputs` / `encode_drought_preimage` / `drought_determination_digest`
(encoding.rs; layout table in §4). `DROUGHT_PARAMS_HASH_RDI_1_0 = keccak(DROUGHT_PARAMS_JSON)
= 0x2580af…47cf`, computed by the oracle's own code. The 13 evidence words are
byte-identical to the EVM `abi.encode` (only the sum-insured scale differs): with the EVM
conformance fixture's values the Soroban encoder reproduces its pinned inputs hash
`0xe6e25b76…2976`.

**`PayoutReceiver::submit_drought_determination(caller, d, signature, recovery_id)`** —
checks in order: relayer, not paused (318), touch; signer configured (300); methodology
enabled (322); bounds `bp <= 10000` (301), `drought_damage <= 100` (323), evidence (325:
`lgp_days == 180`, `cdd_w2_days <= 40`, both normals non-zero, every rain value
`<= 1e9`, `season_start_epoch % 86400 == 0`, params hash = rdi-1.0's); `bp == dd * 100`
(324); **on-chain index recompute** (324; not on EVM); `bp >= 3000` (306); policy exists,
ACTIVE, `now <= end_date`, unpaid (307–310); `coverage_type == Drought` (320); sum insured
(311); plot within 1e-6 degree (321); **evidence window** — planting on or after the UTC day
cover started and `season_start + 110 d <= end_date` (326), `assessed_at >= season_start +
110 d` (327); `payout == sum_insured * bp / 10000` floor (312); freshness (313/314); claim
limit (315); digest rebuilt on-chain; not consumed (316); signer recovered (317).

Effects and interactions are the crop path's, factored into `settle_verified`; the report
stores `weather_damage = satellite_damage = 0`; new event `DroughtEvidenceRecorded`.
`PolicyPaid` is shared, so a policy settles at most once across both methodologies
(the third layer was Treasury `PayoutProcessed`; since Option B it is the adapter's one
obligation per (PayoutReceiver, policy)).

**Methodology allow-list.** Instance key `MethodologyEnabled(MethodologyKind)`
(`CropDualIndex10 = 0`, `CropDroughtRdi10 = 1`), absent = disabled,
`set_methodology_enabled` (Admin), `is_methodology_enabled`. `submit_determination` (crop)
checks it too: leave crop disabled after the upgrade.

**View.** `drought_determination_digest(d) -> BytesN<32>` for the oracle and backend to
simulate against the live contract and domain.

**PolicyManager.** `create_policy` sells `Drought` only (118 otherwise). Re-opening
`Comprehensive` needs an audited upgrade once a cause-agnostic methodology is live.

**Rounding.** Payout rounds down. rdi-1.0 emits whole percent (multiples of 100 bp);
measured over a grid of satisfaction ratios, its integer index is biased +2.4 / −1.3 points
against exact arithmetic (up to +240 bp; not strictly round-up). Fixing that is a new
methodology version (new string, backtest, agronomist sign-off), not a contract change.

**Vectors and their sources.**

| Vector | Generated by | Checked by |
|---|---|---|
| V1–V3 drought inputs hash / preimage / digest (`drought_golden_vectors`) | this Rust encoder | rebuilt byte for byte with the oracle's existing `stellar.shared.js` helpers + ethers v6 (throwaway copy); the oracle's Part 3 test must pin them |
| V1 signature r‖s, recovery id 1 | ethers v6 `SigningKey(TEST_PRIVATE_KEY).sign` (oracle test key) | Rust: recovers to that key via the host's `secp256k1_recover` |
| 225-point recompute grid | the oracle's `calculateDroughtDamage` (1bc2d9d) on synthetic archive bodies | `rdi_v1_drought_damage` |
| EVM inputs hash `0xe6e25b76…2976` | Solidity / oracle / backend (`DroughtConformance.t.sol`, contracts@ae163d1) | `encode_drought_inputs` |
| params hash `0x2580af…47cf` | ethers v6 over the oracle's `DROUGHT_PARAMS_JSON` | keccak in Rust |
| real 2022 season, pilot pixel 1.0167 N / 35.0062 E: 42% | the oracle's `calculateDroughtDamage` on the live Open-Meteo ERA5 response (fetched 2026-10-18) | integration test `real_era5_season_settles_end_to_end` |

The EVM conformance fixture (45% from W1 12500/20000, W2 3000/18000) fails the recompute:
those rains give 100%. That fixture is internally inconsistent; it is used here only for
the byte layout.

**Decisions left to the product owner.** (1) Late enrolment: strict by default (activate on
or before planting); a grace period needs a constant and leaks W1 rainfall. (2) The on-chain
recompute hard-codes Ky 40/150, which the pinned params hash already freezes. (3) Coverage at
sale is hard-coded to Drought, not an admin list. (4) Keeper target 30 days for code (60
would halve the keeper's frequency but lose up to twice the prepaid rent on an upgrade).

## 10. Rent cap

**Where the fees came from** (testnet transaction metas). Rent = `ceil(size ×
fee_per_rent_1kb × ledgers / (1024 × 1215))`, code entries divided by 3; code rent size =
XDR size + `wasm_module_memory_cost`. Testnet `fee_per_rent_1kb` = 8,113 stroops, mainnet
1,000 (8.1× cheaper). About 99.9% of each first-touch fee was the **contract code entry**
being extended from its 7-day deploy TTL to the network maximum (~173 more days), because
`ttl.rs` called `extend_ttl(max, max)` on every touch:

| First touch | Charged | Paid for |
|---|---|---|
| PolicyManager.set_policy_nft | 636.61 XLM | PM code, 981,078 rent bytes (16.8× the wasm) |
| PolicyNft.set_policy_manager | 362.36 XLM | NFT code, 558,014 rent bytes |
| Treasury.receive_premium | 832.58 XLM | Treasury code, 1,279,236 rent bytes — on the money path |
| PayoutReceiver.submit_determination | 662.56 XLM | PR code, 1,014,107 rent bytes |
| create_policy / activate_policy | 2.63 / 2.65 XLM | data entries only |

Ongoing rent of the four code entries, unchanged by any TTL policy, at the pre-drought code
sizes: testnet 14.39 XLM/day (432/30 d), mainnet 1.77 XLM/day (53/30 d); about 17 and 2.1
after the growth in §5 (table below). Data entries are cheap (556 B for 180 days:
1.13 XLM testnet, 0.14 mainnet).

**Policy now** (`crates/shared/src/ttl.rs`, 1 day = 17,280 ledgers):

| Class | Rule | Kept alive by |
|---|---|---|
| Instance + code, user path | below 3 d → 10 d (safety net only) | any call (`touch_contract`) |
| Instance + code, keeper | below 23 d → 30 d | `extend_contract_ttl()`: PayoutReceiver's extends itself and the SettlementAdapter (Treasury before Option B) and calls PolicyManager's, which extends PolicyManager and PolicyNft |
| Role grants | below 30 d → 60 d | grant and every use |
| Per-policy: PM `Policy`, `PendingCounted`, farmer index item; Treasury `PremiumReceived`, `PayoutProcessed`; PR `PolicyPaid`, `Report`; NFT `Certificate`, `Owner`, `PolicyToToken`, `DistributorPolicy` | `end_date + 30 d`, clamped; activation re-applies it to PM `Policy` and the shared per-farmer / per-org entries, but not to the farmer index item (`FarmerPolicyAt`, whose position the policy does not record) nor to a `PremiumReceived` set while PENDING: those keep the create-time need, short by the PENDING period, until `extend_policy_ttl` (Treasury's flag) or a restore (index item); a closed policy keeps its TTL (`PayoutProcessed` at least 60 d) | the lifecycle calls; `extend_policy_ttl(id)` when the need is past the maximum |
| Open obligation (Option B): adapter `Obligation`, `ObligationOf`, `OrgObligationAt` while Owed / Funded; `OrgObligationCount` at least 60 d. A Settled obligation is left to archive | below 30 d → 60 d | record, every mark, `extend_obligation_ttl`, and `extend_policy_ttl` (best effort) |
| (Treasury era only) pending claim: `ClaimPending`, `PendingPayout`, `OrgQueueAt/Head/Tail`, `OrgPendingPayouts`, PM `Policy` while CLAIM_PENDING | below 30 d → 60 d | settlement attempts and `extend_policy_ttl` |
| Shared per-farmer / per-org: `Farmer*`, `OrgOutstanding`, `OrgReserve`, ratio and fee settings, `DistributorCount` | longest need of any policy touching it, at least 60 d; `FarmerClaimTimes` 365 d | every touch |
| Temporary `ConsumedDetermination` | 7 d | `PolicyPaid` is the durable guard |

Admin and config calls extend nothing. Paged index views no longer extend what they read.

**Archived-entry safety.** The host refuses an expired persistent entry until it is
restored, so an archived `PolicyPaid`, `PayoutProcessed`, `PremiumReceived`, role grant,
`FarmerClaimTimes`, `OrgOutstanding` or `OrgReserve` never reads as absent or false: it
fails closed, or it is restored and reads its true value. Instance flags (`Paused`, signer,
domain, `MethodologyEnabled`) archive with the whole instance. No security flag is in
temporary storage except the redundant replay marker, and no positive guard (`PolicyPaid`,
`PayoutProcessed`, `PremiumReceived`) is ever `remove()`d. What is removed (revoked role
grants, `ClaimPending`, `PendingPayout`, `OrgQueueAt`, `PendingCounted`) is removed only
when its absence is the correct state.

**Expected fees after the change.** Data-entry rows: measured on the host (the four
optimized wasm builds registered in one `Env` with the testnet ledger limits, rent read from
the host's per-call metering). Code rows: the network's code rent size per contract (XDR
size + `wasm_module_memory_cost`, from the 2026-10-13 testnet metas above) scaled by the
optimized wasm growth in §5, because the local host (soroban-env-host 27) under-reports
module memory cost against the network's protocol 29 cost model. Nothing here is measured
on testnet yet.

| Call | Testnet | Mainnet |
|---|---|---|
| Admin and config calls | ≤ 0.09 XLM (`grant_role`), others ≈ 0.01 | ≤ 0.011 XLM |
| `create_policy` | 2.54 XLM | ≈ 0.31 XLM (mainnet's 120-day creation floor applies) |
| `receive_premium` (Treasury era; removed by Option B) | 832.6 → 0.60 XLM | ≈ 0.07 XLM |
| `activate_policy` | 2.52 XLM | ≈ 0.31 XLM |
| `submit_drought_determination` (Treasury era, pending path) | 662.6 → 1.05 XLM | ≈ 0.13 XLM |
| Code rent per day: PayoutReceiver / Treasury / PolicyManager / PolicyNft | ≈ 5.0 / 5.2 / 4.1 / 2.8 XLM | ≈ 0.62 / 0.64 / 0.50 / 0.34 XLM |

The table predates Option B and is not re-measured. Scaling by optimized size, the
SettlementAdapter's code rent (41,033 B) should be about 0.63× the Treasury's (64,683 B),
roughly 3.3 XLM/day on testnet; the keeper and lapse rows shrink accordingly. Measure on
testnet before relying on these.
| Keeper, all four contracts | ≈ 17 XLM/day, ≈ 119 XLM/week | ≈ 2.1 XLM/day, ≈ 14.7 XLM/week |
| Worst-case user call after a keeper lapse | up to ~10 days of code rent for **each** contract the call touches: a determination touches all four, ≈ 150–170 XLM | ≈ 19–21 XLM |

The lapse row: each contract a call touches tops up separately (below 3 days to 10 days),
so one call can pay for all four. An `extend_policy_ttl` after a lapse was measured on the
host extending all four contracts by 8 days in one call. Alert on keeper failure; lowering
`USER_TARGET_LEDGERS` would cap this at the price of more frequent top-ups.

Mainnet deploy budget: uploads pay the 120-day minimum, about 252 XLM for the four code
entries (was 213 before the growth); the keeper has nothing to do until about day 97.

## 11. Option B: SettlementAdapter

**Why.** Deliverable 2 of the Stellar Instaward SOW asks for "a minimal
FundingFacility/SettlementAdapter stub ... [that] tracks verified obligations and their
funding status without pooling or custodying farmer premiums itself", with the
PayoutReceiver wired to it instead of a Treasury-style contract. It is also the company's
2026-09-09 non-custodial decision (no deposits, no pooled reserves). Option B (decided
2026-10-19) removes the per-org USDC Treasury and puts a small obligation ledger in its
place. Every verification step of the PayoutReceiver is unchanged.

**Flow.**

```
submit_drought_determination  (all checks of §9, unchanged)
  -> effects: ConsumedDetermination (temp), PolicyPaid, Report
  -> adapter.record_obligation(this, policy_id, org, payout, digest) -> Obligation
       checked field by field, else ObligationNotRecorded (329)
  -> pm.mark_as_claimed, pm.increment_claim_count
  -> events DeterminationVerified, DamageReportReceived, ObligationRequested
off-chain: the backend off-ramps the payout (M-Pesa) as paying agent
  -> adapter.mark_funded(funder, id, sha256(order reference))     when the order is accepted
  -> adapter.mark_settled(funder|org, id, sha256(receipt ref))    when the receipt arrives
```

The transaction is atomic: an adapter error (500 unbound, 502 conflict, 3 not configured)
reverts the determination's effects too, so it can be resubmitted after the fix.

**State machine.** `Owed → Funded → Settled`, or `Owed → Settled` (funding may be
implicit). Status never moves backwards. A repeated mark with the same reference is a no-op
(webhooks arrive twice); with a different reference it is refused (`ObligationAlreadyFunded`
504, `ObligationAlreadySettled` 505); any transition out of `Settled` except the identical
`mark_settled` is 505; an all-zero reference is 506. Disputed and Waived are deliberately left
out: a waive by the obligor is self-dealing, and a dispute needs an adjudicator role. Adding
enum variants later is forward-compatible.

**Attestation, not proof (read this before citing a `Settled`).** Organisation addresses are
keyless: the backend derives them as `sha256("microcrop:org:stellar:v1:" + orgId)`, so nobody
holds the key and the "org signs `mark_settled`" path cannot be used for backend-onboarded
orgs. In practice every transition is made by `Role::Funder`, held by the backend account.
`Funded` and `Settled` are therefore the paying agent's attestations, identified by a
reference hash of the off-chain record (no phone number or name ever goes on-chain). The
events, the reference hashes and the revocable role are the audit trail; nothing on-chain
proves money moved.

**Keying.** Policy ids are unique only per PolicyManager. Obligations are looked up by
`ObligationOf(payout_receiver, policy_id)`, so pointing the adapter at a new PayoutReceiver
never collides with the old one's obligations (which would otherwise fail 502 and brick new
claims). Within one PayoutReceiver there is at most one obligation per policy (an identical
re-record returns the existing one, no write, no event; anything else is 502). Across
PayoutReceivers sharing one PolicyManager, the policy's `Claimed` status (308) stops a second
claim. Replay after a PayoutReceiver → adapter swap is still 310, because `PolicyPaid` lives
in the PayoutReceiver.

**Binding.** `PayoutReceiver::set_settlement_adapter(caller, adapter)` (Admin) accepts only a
contract whose `payout_receiver()` returns this PayoutReceiver; anything else, the
PayoutReceiver itself, or a non-adapter contract is `SettlementAdapterNotBound` (328). It is
not pause-gated (an incident swap). `SettlementAdapter::set_payout_receiver` (Admin) may
rotate at any time; the old PayoutReceiver's next record fails 500 and its obligations stay
markable. Until the first binding, determinations fail closed (3 / 500); `deploy.sh` and
`verify-deployment.sh` enforce it.

**Storage and TTL.** Instance: `PayoutReceiver`, `NextObligationId`. Persistent:
`Obligation(id)`, `ObligationOf(PR, policy)`, `OrgObligationAt(org, i)` below 30 d → 60 d on
every write and by `extend_obligation_ttl` while not Settled; `OrgObligationCount(org)` at
least 60 d. A Settled obligation is left to archive: it is restorable, and an archived entry
fails closed (the host refuses the read), so record and mark trap rather than recreate it
fresh. Every state-changing entrypoint calls `touch_contract` (below 3 d → 10 d);
`extend_contract_ttl` keeps it alive (below 23 d → 30 d), and `PayoutReceiver::
extend_contract_ttl` covers the adapter. `get_org_obligations` pages are capped at 50.

**Removed.** `crates/treasury` (code, tests, snapshots); `ITreasury`; `TreasuryError`;
`PolicyManager::mark_claim_pending` and `PolicyClaimPending`; `PayoutReceiver::
settle_pending_claim`, `is_claim_pending`, `ClaimPending(u64)`, and the events
`PayoutInitiated`, `ClaimAwaitingFunding`, `PendingClaimSettled`; `receive_premium` (the
premium proof stays off-chain, the M-Pesa reference); the USDC SAC everywhere.

**Retired codes (never reuse).** Errors 200..=226 (the whole 200..=299 range, 215/216
included), `NoPendingClaim` 319 (variant kept, never raised), `PolicyStatus` ordinal 5
(`ClaimPending`; the backend keeps it for decoding legacy deployments), `Role` 3 (`Payout`;
variant kept). New: `Role::Funder = 7`, `ObligationStatus {Owed 0, Funded 1, Settled 2}`,
`PayoutError` 328/329, `AdapterError` 500..=507. All pinned by `shared/src/abi_tests.rs`.

**Testnet-only fixture.** `crates/testnet-policy-fixture` (`TestnetPolicyFixture`) implements
the `IPolicyManager` methods the PayoutReceiver calls; its admin injects policy state with
`set_policy` (not a sale), and its constructor panics on the mainnet network id. It is a
workspace member but not a default member, so `deploy.sh` never builds or deploys it. It
exists for `scripts/testnet-evidence.sh` (runbook *Testnet evidence*).

**Why positive evidence on the real contracts takes a season.** `activate_policy` sets
`start_date = now`; a drought determination needs `season_start >= start_day`,
`assessed_at >= season_start + 110 d` (327), and freshness `now <= assessed_at + 3600`. So
the earliest positive result on a real sale is 110 days after activation (~117 with the
archive lag; around Feb 2027 if activated now, and Jul–Sep 2027 through the backend's KE
long-rains sale gate). The crop path settles `Comprehensive` only (320), which `create_policy`
refuses (118). Earlier testnet runs (`deploy-v2`) hit exactly #327, #326, #322 and #118.
Hence the two evidence lanes: Lane P (real contracts, stops at #327 before signature
recovery) and Lane E (identical WASM on the fixture, a real 2022 ERA5 season signed by a
throwaway key). Testnet resets roughly quarterly may intervene before 110 days; a durable
production-lane proof may need mainnet with a tiny sum insured. A real M-Pesa receipt behind
`mark_settled` needs a real payout.

**Stellar skill checklists** (`stellar-dev` smart-contracts skill, `security.md` and
`testing.md`). Satisfied by this design:

- privileged functions authorised against storage: `record_obligation` = stored
  PayoutReceiver + `require_auth`; marks = stored `ob.org` or the stored Funder grant;
  `set_payout_receiver`, `grant_role`, `revoke_role`, `set_settlement_adapter` = Admin;
  `upgrade` = Upgrader;
- re-auth at every layer: the PayoutReceiver passes its own address and the adapter
  `require_auth`s it; marks `require_auth` the caller;
- initialisation once (`__constructor`);
- external calls allowlisted and return values checked: the adapter is admin-set, bound by
  handshake (328) and its return value is checked (329); the adapter makes no external calls
  (classes 4, 9);
- arithmetic: `amount > 0`, ids and counts `checked_add`, `overflow-checks = true` (class 5);
- typed `#[contracttype]` storage keys (class 6);
- TTL: persistent obligations extended on touch plus a keeper; no temporary storage in the
  adapter, so no TTL-as-security (class 8);
- bounded loops: pages capped at 50 (class 10);
- events for every obligation transition and config change; no error code renumbered;
- upgrade gated and tested; pause not applicable (the adapter is not value-bearing; the
  PayoutReceiver pause stays and stops new obligations);
- tests: `try_` error paths, edge amounts (0, −1, `i128::MIN`), `env.auths()` trees and
  `set_auths(&[])`, `to_xdr` event equality, `get_ttl`, real-contract integration plus an
  `#[ignore]`d real-WASM lane, a proptest, committed snapshots.

**Flags against the checklists (open):**

1. No `.github` CI and no SEP-55 release or build attestation; `deploy.sh` deploys a local
   build, the skill says to deploy the release artifact. The repo is public, so the
   attestation workflow is available. `verify-deployment.sh` step 7 (on-chain wasm hash ==
   local optimized build) is a stand-in.
2. Auditable state changes without events in every contract: `grant_role`, `revoke_role`,
   `set_network_domain`, `pause`, `unpause`, `upgrade`. Fix in `shared/roles.rs` plus the
   setters, as a follow-up.
3. `upgrade` refusal tests now exist for PolicyManager, PayoutReceiver and the adapter; no
   crate has a happy-path upgrade test (needs a second WASM).
4. One key (`alice` on testnet) holds Admin and Upgrader everywhere. With a settable adapter,
   Admin can redirect obligations, which equals Upgrader power. Mainnet should use a multisig
   or custom account and a timelock on `set_settlement_adapter` (D-B3).
5. `require_plot` uses an unchecked `(i128 - i128).abs()`; it traps instead of returning 321.
   Low severity; prefer `checked_sub` plus the error (class 5, "fails cleaner").
6. Attestation centralisation: the Funder can mark any obligation settled. Mitigated by the
   reference-hash trail, revocable roles and events; documented, not solved.
7. Integration tests register native rlib contracts; the `#[ignore]`d `wasm_lane_full_flow`
   covers the compiled WASM and must be run explicitly.
8. Optional hardening: the PayoutReceiver could require `NetworkDomain ==
   env.ledger().network_id()`, removing an admin misconfiguration class.

**Open decisions (product owner).**

- **D-B1, who pays and when.** Default here: MicroCrop off-ramps as paying agent as soon as
  the obligation is `Owed`; `Funded` = off-ramp order accepted, `Settled` = M-Pesa receipt.
  MicroCrop carries the credit exposure until the partner reimburses off-chain. Alternative:
  hold the off-ramp until the partner remits (`Funded` = remittance confirmed by ops).
- **D-B2, Tier 1 on Stellar.** A partner-attested trigger (`PartnerSettlementReport` →
  `mark_settled`) needs Tier 1 sales on Stellar and sign-off to relax the backend's
  partner-settlement / money-path separation guard.
- **D-B3, mainnet governance.** Multisig for Admin/Upgrader, and a timelock on
  `set_settlement_adapter`, before any mainnet deploy.
