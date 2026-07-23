# MicroCrop: EVM → Soroban Migration Notes

**Honest framing:** this workspace is a **preparation port**, not a completed migration.
The four contracts are faithfully re-implemented in Rust, compile to `wasm32v1-none`
WASM, and pass their unit suites (66 tests). They have **not** been deployed, integrated
with the off-chain stack, security-audited on Soroban, or reconciled with live state.
The [DEFERRED](#deferred--not-yet-done) section is the load-bearing part of this document.

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
Preserved 1:1: sum/premium/duration bounds; combined open-policy cap (ACTIVE+PENDING ≤ 5,
Finding 5); PENDING→ACTIVE window reset preserving duration; per-org **outstanding
exposure** accounting (accrue on activate, release on claim/cancel/expire) that drives
Treasury solvency; claim cap (≤ 3/farmer/year, year = `ts / (365*86400)`); id starts at 1;
`PendingCounted` flag (Finding 8). NFT status updates are **best-effort** via the fallible
`try_update_policy_status` client (Soroban equivalent of Solidity `try/catch`) so lifecycle
transitions can never be bricked. Roles: create/activate/cancel/expire = `Backend`;
mark_as_claimed/increment_claim_count = `Oracle`; setters = `Admin`; upgrade = `Upgrader`.

### Treasury (`crates/treasury`) ← Treasury.sol v3.1.0
Money math preserved 1:1: premium split `fee = amount * fee_bps(org) / 10_000`;
per-org reserve credit; `receive_premium` pulls USDC from caller (`TokenClient::transfer`);
`request_payout` funds **only** from the policy's org reserve (`InsufficientOrgReserve` if
short); surplus-only withdrawals (`WouldBreachReserve`); `reserve_required` read from
PolicyManager's `org_outstanding_sum_insured`; `emergency_withdraw` touches only *unbacked*
surplus (`balance − Σ reserves − fees`). CEI ordering (effects before the USDC interaction)
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
Treasury `PayoutProcessed`). The contract passes `env.current_contract_address()` as the
`caller` to its Treasury/PolicyManager sub-invocations (a contract auto-authorizes its own
sub-calls).

### PolicyNFT (`crates/policy-nft`) ← PolicyNFT.sol
`tokenId == policyId`; `policy_id == 0` rejected (reserved sentinel); re-mint rejected;
**soulbound while active** (`transfer` reverts `TransferWhileActive` if `cert.is_active`);
`update_policy_status` callable **only** by the stored PolicyManager address (Finding 6);
distributor index preserved. Minter role gates `mint_policy`.

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
   contract holds SAC balances directly (no trustline provisioning in-contract).
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
- [ ] **No end-to-end / cross-contract integration test.** Each crate tests in isolation
      with *in-file mock* siblings (PolicyManager mocks the NFT; PayoutReceiver mocks
      Treasury + PolicyManager). A real deploy-all-four → premium → determination → payout
      integration test on a shared `Env` is not yet written.
- [ ] **USDC SAC address** for each target network is not wired; deploy scripts/aliases,
      role-granting runbook, and the wiring sequence are documented in the README but not
      automated (no `Makefile`/deploy script).
- [ ] **`network_domain` value not chosen**, and the **Lit PKP off-chain signer has not been
      updated** to produce Soroban-format digests (keccak over `shared::encoding`,
      `network_domain` instead of chainid, XDR contract address instead of `address(this)`).
      Until both sides agree byte-for-byte, no real determination will verify.
- [ ] **Backend / indexer / relayer integration** untouched — the EVM stack (ethers.js
      listeners, Ponder indexer, relayer) still targets Base. Ledger events, XDR decoding,
      and Stellar RPC are not wired.

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
- [ ] `cargo clippy` was not run (clippy component not installed in this environment); a lint
      pass is deferred.

**Product parity not ported**
- [ ] On-chain `tokenURI` rendering (intentionally off-chain now) — needs an off-chain
      renderer before any NFT marketplace/explorer integration.
- [ ] Legacy migration shims (`setLegacyPolicyOrg`, storage `__gap`) — intentionally omitted;
      irrelevant on a fresh chain but noted so no one expects state import from Base.

---

## 6. Bottom line

The Rust/Soroban re-implementation is **complete and green** for a first version: it builds
to WASM on the correct target and all 66 unit tests pass, with the five security-critical
invariants preserved. It is **ready to be deployed to testnet and integrated next** — it is
**not** yet a live migration. Do the DEFERRED §5 work (starting with the shared
`network_domain` + Lit PKP signer agreement and an all-four integration test) before any
mainnet consideration.
