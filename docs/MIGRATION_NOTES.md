# MicroCrop: EVM → Soroban Migration Notes

**Honest framing:** this workspace is a **preparation port**, not a completed migration.
The four contracts are faithfully re-implemented in Rust, compile to `wasm32v1-none`
WASM, and pass their unit and integration suites (118 tests). They have **not** been
deployed, integrated with the off-chain stack, or reconciled with live state. An AI-assisted
Soroban review (2026-10) found 11 issues; all are fixed on this code (see
[§6](#6-audit-fixes-2026-10--where-the-port-now-departs-from-the-solidity)), but that review
is not a substitute for an independent audit.
The [DEFERRED](#deferred--not-yet-done) section is the load-bearing part of this document.

**Product-architecture decision (2026-08-21):** the faithful Treasury port is now a
legacy/reference implementation, not the target mainnet design. The carrier will keep
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
| ERC-20 USDC (`transfer`, allowances)             | SEP-41 Stellar Asset Contract via `soroban_sdk::token::TokenClient` (SAC balance, **no trustline needed** for the contract) |
| UUPS `upgradeToAndCall`                          | `env.deployer().update_current_contract_wasm(hash)` behind an `Upgrader`-role `upgrade()` fn |
| `try/catch` on external call                     | generated client's `try_*` method returning `Result`                                        |
| cross-contract call (interface)                  | `#[contractclient]` trait → generated `...Client` (`shared/interfaces.rs`)                   |
| revert on overflow                               | `overflow-checks = true` in the release profile (money i128 traps, never wraps)             |

---

## 2. Per-contract mapping & what is PRESERVED

### PolicyManager (`crates/policy-manager`) ← PolicyManager.sol v3.1.0
Preserved: duration bounds; PENDING→ACTIVE window reset preserving duration; per-org
**outstanding exposure** accounting that drives Treasury solvency; id starts at 1;
`PendingCounted` flag (Finding 8). NFT status updates are **best-effort** via the fallible
`try_update_policy_status` client (Soroban equivalent of Solidity `try/catch`) so lifecycle
transitions can never be bricked. Changed by the 2026-10 audit fixes (§6): sum bounds in
7dp; exposure accrues at **create** (release on cancel/expire/claim); open cap is
`open (ACTIVE+PENDING) + claims in the rolling window ≤ 3`; claim cap is 3 per rolling
365 days (not a calendar-year bucket); farmer index is paginated; new `ClaimPending`
status. Roles: create/activate = `Backend`; cancel PENDING = `Backend` or `Admin`, cancel
ACTIVE = `Admin` only; expire = anyone after `end_date`;
mark_as_claimed/mark_claim_pending/increment_claim_count = `Oracle`; setters = `Admin`;
upgrade = `Upgrader`.

### Treasury (`crates/treasury`) ← Treasury.sol v3.1.0
Money math preserved 1:1: premium split `fee = amount * fee_bps(org) / 10_000`;
per-org reserve credit; `receive_premium` pulls USDC from caller (`TokenClient::transfer`);
`request_payout` funds **only** from the policy's org reserve; surplus-only withdrawals
(`WouldBreachReserve`); `emergency_withdraw` touches only *unbacked* surplus
(`balance − Σ reserves − fees`). Changed by §6: a short reserve records a **pending
payout** instead of reverting (`settle_pending_payout` pays it later);
`reserve_required = outstanding × ratio + pending payouts`; reserve ratio default and
floor 30%; `receive_premium` only for PENDING, or ACTIVE-and-not-ended, policies; the
constructor refuses a token whose `decimals() != 7`. CEI ordering (effects before the USDC interaction)
kept. Solvency invariant `balance ≥ total_org_reserves + accumulated_fees` asserted in tests.
Roles: `receive_premium` = `Backend`; `request_payout` = `Payout` **and** `caller ==
payout_receiver`; reserve/fee setters = `Admin`.

### PayoutReceiver (`crates/payout-receiver`) ← PayoutReceiver.sol v2.1.0 (security crux)
The full **12-step validation runs in the exact Solidity order**, then CEI effects, then
interactions. Signature is the **sole** payout authority; `Relayer` is anti-spam only.
Preserved: bounds/consistency checks; weighted invariant with **no division** (`60w+40s==bp`,
satellite-only `100s==bp`); threshold 3000; `sum_insured` match; **payout re-derived
on-chain** and compared to the signed value; freshness (`MAX_REPORT_AGE=3600`, saturating);
farmer claim limit; on-chain digest reconstruction via `shared::encoding::
determination_digest`; `secp256k1_recover` byte-compared to the stored SEC-1 65-byte key;
**triple replay guard** (temporary `ConsumedDetermination` + persistent `PolicyPaid` +
Treasury `PayoutProcessed`). Added by §6: when Treasury reports a short reserve the policy
becomes `ClaimPending` and the permissionless `settle_pending_claim(policy_id)` pays it in
full later. The contract passes `env.current_contract_address()` as the
`caller` to its Treasury/PolicyManager sub-invocations (a contract auto-authorizes its own
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
3. **USDC = SEP-41 Stellar Asset Contract**, not ERC-20. Balances via `TokenClient`; the
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
7. **Storage TTL / rent.** Every persistent/instance/temporary entry gets an explicit
   `extend_ttl` bump (persistent ~30–90d, instance ~7–30d, temporary short) — there is no
   EVM equivalent; entries can be archived if TTL lapses.
8. **Events use `#[contractevent]`.** `env.events().publish(...)` is deprecated in SDK 27;
   `#[topic]` fields replace Solidity `indexed`. Some zero-value "previous value" event
   fields were dropped (no zero Address to emit).
9. **Pausable error codes.** OpenZeppelin `Pausable` has no code in the frozen shared
   `TreasuryError`/`PayoutError` ranges, so Treasury and PayoutReceiver each define a small
   crate-local `#[contracterror]` paused code (`215/216` and `318` respectively). See §5.
10. **Legacy shims omitted.** `setLegacyPolicyOrg`, `__gap`, `__deprecated` storage padding
    are gone — this is a fresh chain with no state to migrate or storage layout to preserve.

---

## 4. Frozen cross-contract contract (do not drift)

`crates/shared` is the coordination boundary; changing it requires re-checking all four
contracts. In particular **`encoding.rs` is FROZEN**: the determination byte layout
(`SCHEMA_VERSION="1.0"`, `KIND="CROP_DAMAGE"`, `METHODOLOGY="crop-dualindex-1.0"`, 10
evidence words + 6 result words, big-endian two's-complement, `network_domain` in place of
`block.chainid`, contract XDR bytes in place of `address(this)`) must byte-for-byte match
the off-chain signer. `interfaces.rs` client signatures must match the real inherent methods.

---

## 5. DEFERRED / NOT-YET-DONE (be honest)

These are known gaps. None are "bugs" in the ported logic; they are the work that turns a
preparation build into a real migration.

**Integration & deployment**
- [ ] **Not deployed** anywhere (no testnet, no mainnet). No contract addresses exist.
- [x] ~~**No end-to-end / cross-contract integration test.**~~ DONE — `crates/integration-tests`
      registers all four REAL contracts on one `Env` (no mocks) and covers the full
      premium → determination → payout lifecycle, plus determination replay, wrong-signer
      rejection and the per-org reserve bound. Still HOST-SIDE only: running this flow against
      live testnet, with real transaction hashes, remains open.
- [x] ~~deploy script / role-granting runbook not automated~~ DONE — `scripts/deploy.sh`
      (idempotent, resumable, dry-run, mainnet-gated) and `scripts/verify-deployment.sh`
      (exact role checks via the new `has_role` view) plus `docs/DEPLOYMENT_RUNBOOK.md`.
- [ ] **USDC SAC address** for each target network is still not wired. It is a required
      constructor argument for Treasury, supplied via `USDC_SAC` to the deploy script.
- [x] ~~`network_domain` value not chosen~~ DECIDED — derived as `sha256(network passphrase)`:
      testnet `cee0302d…ecd472`, mainnet `7ac33997…45a979`. It does what `block.chainid` did
      (binds a determination to one network) and both sides compute it from a public constant,
      so no magic number has to be shipped. Set by `scripts/deploy.sh`; overridable.
- [ ] **The Lit PKP off-chain signer has not been updated** to produce Soroban-format digests
      (keccak over `shared::encoding`, `network_domain` instead of chainid, XDR contract address
      instead of `address(this)`). Until both sides agree byte-for-byte, nothing verifies. Note
      the signer needs the PKP's 65-byte SEC-1 PUBLIC KEY, not its 0x address — the deploy
      script rejects an address, because passing one silently fails every signature check.
- [ ] **Backend / indexer / relayer integration** untouched — the EVM stack (ethers.js
      listeners, Ponder indexer, relayer) still targets Base. Ledger events, XDR decoding,
      and Stellar RPC are not wired.

- [ ] **The signed preimage is hardcoded to ONE methodology and ONE evidence schema.**
      `METHODOLOGY_CROP = b"crop-dualindex-1.0"` is a keccak domain constant, and the ten
      `encode_inputs` evidence fields are dual-index-shaped (`ndvi_scaled`, `weather_temp_c_e2`,
      `weather_precip_e2`, `weather_humidity`, `weather_wind_e2`). The drought methodology's
      actual evidence is completely different (`rainW1_e2`, `rainW2_e2`, `rainRefW1_e2`,
      `rainRefW2_e2`, `cddW2Days`, `lgpDays`, `seasonStartEpoch`), which is why adding drought
      needed a separate PayoutReceiver path. **Every new methodology therefore costs a contract
      upgrade plus an audit.** Parameterising the methodology domain and generalising the
      evidence encoding is nearly free before deployment and an upgrade afterwards.
- [ ] **There is no observation-source field anywhere in the signed preimage** — no dataset, no
      product version, no served grid cell. The determination-stability work
      (microcrop-context/docs/DETERMINATION_STABILITY.md) found the same farmer gets a different
      payout band on 39% of seasons under RDI, and 86% under CDD, purely from the choice of
      rainfall product. The leading remedy is to declare a canonical source and SIGN it — which
      the current on-chain schema cannot express at all. The off-chain canonical determination
      can (it carries `methodologyVersion` and `provenance.url`); the on-chain verifier cannot.

**Verification & safety**
- [ ] **No Soroban security audit.** The Solidity was audited; this Rust port has not been.
      The secp256k1 signature path, replay guards, and per-org solvency need fresh review on
      Soroban semantics (auth trees, TTL/archival edge cases).
- [ ] **TTL/rent policy is placeholder.** Extend-thresholds were chosen for tests, not
      tuned to real ledger economics; an archived `PolicyPaid`/`ConsumedDetermination` entry
      has replay-safety implications that must be analysed before mainnet.
- [ ] **Shared paused-code fragmentation** (§3.9): recommend adding one shared paused error
      to `microcrop_shared::errors` so all contracts share a code, instead of three local
      enums. Deferred to avoid touching the frozen shared crate mid-port.
- [ ] **`org` resolution panic semantics** (§3.1): `policy_org(unknown)` panics rather than
      returning a sentinel; confirm no Soroban caller depends on the old address(0) behavior.
- [ ] **Fuzz / property tests** for the payout math and the encoder byte layout are not
      written (the encoder is the single most replay-sensitive surface).
- [x] ~~`cargo clippy` was not run~~ — now run as a local gate. 7 warnings pre-date the
      audit fixes (duplicated test attributes, two needless borrows, one manual range
      check); the fixes added none. `cargo fmt --all -- --check` is clean.
- [ ] **Lit signer amount scaling.** `microcrop-lit-oracle/lit-actions/crop-oracle.js:231`
      scales `sumInsured` by `1e6`. The Stellar path must scale by `1e7` while the Base path
      stays at `1e6`, so the change has to be chain-aware. Until it ships, every Stellar
      determination fails `SumInsuredMismatch`.
- [ ] **TTL against real network limits.** Per-policy flags now request ~400 days, roles
      ~90 days on grant and use, but the host clamps to the network `max_entry_ttl`, which
      on mainnet is shorter than a 365-day policy. Entries then archive and are restored on
      access; measure the restore cost inside the relayer's 3600 s freshness window.
- [ ] **Pending payouts are not earmarked against new claims.** While an org has a pending
      payout, a later claim that fits in the reserve is still paid at once, which can delay
      the older pending one. `reserve_required` stops the org from withdrawing the money;
      claim ordering between farmers is not enforced on-chain.

**Product parity not ported**
- [ ] **Non-custodial target contracts** — replace mandatory `Treasury` custody with
      `PolicyRegistry`, `PayoutObligation`, `FundingFacility`, and `SettlementAdapter`.
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
| 9, 11 | a short reserve reverted a valid claim with no record; it then lapsed | **pending claims**: Treasury records the full payout, the policy becomes `ClaimPending` (cannot expire, be cancelled, or be settled by another determination), `reserve_required` includes it, and anyone can call `PayoutReceiver.settle_pending_claim(policy_id)` once the reserve covers it | `request_payout -> bool`; new `settle_pending_payout`, `get_pending_payout`, `org_pending_payouts`, `mark_claim_pending`, `settle_pending_claim`, `is_claim_pending`; `PolicyStatus::ClaimPending = 5`; errors 221, 222, 319 |

Leads fixed alongside: role grants extend their TTL on grant and on use; per-policy flags
in Treasury and PayoutReceiver request ~400 days instead of 90 / 30.

## 7. Bottom line

The Rust/Soroban re-implementation is **complete and green** as a parity/reference
version: it builds to WASM on the correct target and its tests preserve the old model's
security invariants. It is suitable for testnet experiments, but the custody Treasury
must not be treated as the final carrier architecture. Freeze and implement the
non-custodial obligation/facility interfaces, local-currency model, Logos Storage
commitment, shared `network_domain`, and Lit PKP signer agreement before any live pilot
or mainnet consideration.
