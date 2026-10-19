# microcrop-soroban

A Soroban (Stellar smart-contract) **preparation port** of the MicroCrop parametric
crop-insurance protocol — a faithful Rust/WASM re-implementation of the four EVM
contracts in `microcrop-contracts/microcrop/src/`.

> **Status: preparation build, not a migration.** All four contracts compile to WASM,
> the full domain logic is implemented, and each crate ships a unit-test suite that
> passes. A first testnet deployment exists; the contracts have **not** been audited on
> Soroban, deployed to mainnet, or wired to the off-chain stack (Lit PKP signer, backend,
> indexer). See [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) for the EVM→Soroban
> mapping and the honest DEFERRED list.

> **2026-10-18 — drought path and rent cap.** PayoutReceiver settles CROP_DROUGHT
> determinations (`crop-drought-rdi-1.0`, the methodology the production signer signs) via
> `submit_drought_determination`; `create_policy` sells `Drought` cover only; the retired
> CROP_DAMAGE path is behind a methodology switch, off by default. Storage rent is capped:
> ordinary calls no longer prepay months of code rent, and a permissionless keeper
> (`extend_contract_ttl`) keeps the contracts alive. The Lit drought action still has to sign
> the Soroban preimage before anything settles (MIGRATION_NOTES §5, §9, §10).

> **Target architecture update (2026-08-21):** This repository faithfully preserves
> the old per-org USDC custody model so it can be tested and migrated safely, but that
> model is no longer the intended commercial architecture. Do **not** deploy the
> `Treasury` port as the final carrier-funding design. The next contract iteration will
> be local-currency-first and separate `PolicyRegistry`, `PayoutObligation`,
> `FundingFacility`, and `SettlementAdapter`. Carrier statutory reserves remain on the
> carrier's books; only a bounded payout facility funds verified obligations. Logos
> Storage is planned for encrypted evidence bundles, with CID/hash commitments recorded
> in the obligation layer.

## Contract overview

| Solidity source (v)        | Soroban crate              | Contract struct  | Role |
| -------------------------- | -------------------------- | ---------------- | ---- |
| `PolicyManager.sol` v3.1.0 | `crates/policy-manager`    | `PolicyManager`  | Policy lifecycle (create → activate → claim/cancel/expire), per-farmer caps, per-org outstanding-exposure accounting. |
| `Treasury.sol` v3.1.0      | `crates/treasury`          | `Treasury`       | USDC custody, premium split (per-org fee), **per-org reserves + on-chain solvency**, payouts, fee withdrawal. |
| `PayoutReceiver.sol` v2.1.0 + v2.2.0 drought path | `crates/payout-receiver`   | `PayoutReceiver` | **Security crux.** Verifies a PKP-signed parametric determination (CROP_DROUGHT; CROP_DAMAGE retired), recomputes the index and re-derives the payout on-chain, triple replay guard, then dispatches the payout. |
| `PolicyNFT.sol`            | `crates/policy-nft`        | `PolicyNft`      | Soulbound-while-active policy certificate + ownership/distributor registry (`tokenId == policyId`). |
| shared types/errors/roles  | `crates/shared` (`microcrop_shared`) | — | `#[contracttype]` structs, `#[contracterror]` codes, the `roles` access-control registry, cross-contract `#[contractclient]` traits, the **frozen** determination byte encoders (crop and drought), the rdi-1.0 index recompute, and the TTL (rent) policy. |

Every contract depends on `microcrop-shared`. The shared crate's `interfaces.rs`,
`encoding.rs`, `types.rs` and `errors.rs` are the frozen cross-contract contract — the
byte layout in `encoding.rs` in particular MUST match the off-chain Lit PKP signer.

## Toolchain (verified against live Stellar/crates.io docs, 2026-07-23)

- **soroban-sdk `=27.0.2`** — current stable on crates.io, published 2026-07-23
  (confirmed: <https://crates.io/crates/soroban-sdk>, <https://docs.rs/soroban-sdk/27.0.2/>).
- **Rust 1.91.0** — the SDK MSRV; pinned in `rust-toolchain.toml` (auto-installed by rustup).
- **Target `wasm32v1-none`** — the current Soroban WASM target. `wasm32-unknown-unknown`
  is **rejected** by soroban-sdk 27.x on Rust ≥ 1.82 (it enables `reference-types` /
  `multi-value` the Soroban host does not support). Use `wasm32v1-none`.
- **Stellar CLI ≥ 22** for `stellar contract build|optimize|deploy|invoke`
  (install: `cargo install --locked stellar-cli` or `brew install stellar-cli`).

## Build

```sh
# 1. Native compile-check of all crates (fast, host arch):
cargo build

# 2. Unit tests (native, uses the soroban-sdk `testutils` host):
cargo test

# 3. Production WASM artifacts:
rustup target add wasm32v1-none        # one-time (rust-toolchain.toml also lists it)
cargo build --target wasm32v1-none --release
#   -> target/wasm32v1-none/release/microcrop_policy_manager.wasm
#      target/wasm32v1-none/release/microcrop_treasury.wasm
#      target/wasm32v1-none/release/microcrop_payout_receiver.wasm
#      target/wasm32v1-none/release/microcrop_policy_nft.wasm

# ...or, with the Stellar CLI (builds every crate + can optimise):
stellar contract build
stellar contract optimize --wasm target/wasm32v1-none/release/microcrop_treasury.wasm
```

> **Note on the requested command.** `cargo build --target wasm32-unknown-unknown
> --release` is documented above as the "4 contracts to WASM" step, but soroban-sdk
> 27.0.2 hard-errors on that legacy target (see build.rs panic: *"use 'wasm32v1-none'"*).
> The equivalent supported command is `cargo build --target wasm32v1-none --release`,
> which produces all four `.wasm` files. This is the target `rust-toolchain.toml` pins.

## Test

```sh
cargo test                              # all crates
cargo test -p microcrop-treasury        # a single contract (note: package name, not path)
```

Package names (for `-p`): `microcrop-shared`, `microcrop-policy-manager`,
`microcrop-treasury`, `microcrop-payout-receiver`, `microcrop-policy-nft`.

## Current build & test status (2026-10-18)

```
cargo build --target wasm32v1-none --release   ->  4/4 WASM contracts built
                                     release     after `stellar contract optimize`
    microcrop_policy_manager.wasm    64,003 B    52,602 B
    microcrop_treasury.wasm          79,856 B    64,683 B
    microcrop_payout_receiver.wasm   72,960 B    59,816 B
    microcrop_policy_nft.wasm        45,714 B    38,481 B

cargo test --workspace                         ->  220/220 passed, 0 failed
    microcrop-payout-receiver   78 passed   (crop + 44 drought: every check in order, real secp256k1 round-trip, cross-methodology replay, pending claims, archived-flag safety, rent cap, signatures required without mock auths)
    microcrop-treasury          49 passed   (premium split, per-org solvency, FIFO pending queue, frozen wallet, receiver-switch guard, 7dp, TTL targets)
    microcrop-policy-manager    45 passed   (lifecycle, caps, claim window, exposure, roles, plot coords, Drought-only sale with a 117-day floor, TTL targets)
    microcrop-integration-tests 21 passed   (all four real contracts on one Env, drought path, real 2022 ERA5 season end to end, keepers)
    microcrop-policy-nft        14 passed   (mint, soulbound transfer, PM-only status, paging, TTL targets)
    microcrop-shared            13 passed   (drought golden vectors, EVM + JS cross-checks, rdi-1.0 grid, TTL helpers)

cargo clippy --workspace --all-targets         ->  7 warnings (all pre-existing)
cargo fmt --all -- --check                     ->  clean
```

On macOS without an accepted Xcode licence, build with the Command Line Tools:
`DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --workspace`.

## Security-critical invariants (preserved from the Solidity)

1. **Determination signature is the sole payout authority.** `submit_drought_determination`
   (and the retired `submit_determination`) reconstructs the digest on-chain via
   `shared::encoding`, bound to the on-chain policy id, the USDC amounts, this contract and
   the network domain, calls `env.crypto().secp256k1_recover`, and byte-compares the
   recovered SEC-1 65-byte key to the stored `authorized_signer`. The `Relayer` role is
   anti-spam only. Only methodologies the admin enabled are accepted (CROP_DROUGHT).
2. **Payout math re-derived on-chain.** `payout == sum_insured * damage_bp / 10_000`
   (floor, overflow-trapping i128), threshold `bp ≥ 3000`, freshness `MAX_REPORT_AGE =
   3600`. For CROP_DROUGHT, `bp == drought_damage * 100` and the rdi-1.0 index is
   recomputed from the signed rainfall evidence; the policy must be `CoverageType::Drought`;
   the season's 110-day window must lie inside the cover, with planting on or after the day
   cover started. The determination must also be about the insured plot (signed
   `latitude_e6` / `longitude_e6` within 1 micro-degree of the coordinates `create_policy`
   stored). `create_policy` sells `Drought` only (`CoverageNotOffered`, 118), so cover that
   could never pay is never sold.
3. **Triple replay guard.** temporary `ConsumedDetermination(digest)` + persistent
   `PolicyPaid(policy_id)` + Treasury `PayoutProcessed(policy_id)`.
4. **Per-org solvency.** Payouts debit only the policy's org reserve. If it is short, the
   verified claim is recorded as a pending payout for the full amount (never dropped,
   never paid partially) and `settle_pending_claim` pays it once the reserve covers it;
   an org's pending claims settle strictly first-in-first-out (only the queue head,
   `Treasury::pending_payout_head(org)`); a USDC transfer the backend wallet cannot receive
   (issuer freeze, missing trustline) is queued the same way instead of reverting;
   a new claim is paid at once only from the reserve not owed to pending claims;
   `receive_premium` takes exactly the policy's `premium`;
   `reserve_required = outstanding_sum_insured * ratio_bps / 10_000 + pending payouts`,
   with a 30% ratio floor; withdrawals are surplus-only; the invariant
   `usdc.balance(this) >= Σ org_reserve + accumulated_fees` holds after every op.
   Amounts are USDC base units at **7 decimals** (the Stellar Asset Contract).
5. **Storage rent is capped.** Ordinary calls extend instance + code only below 3 days, to
   10 days; data entries live as long as the policy needs them (`end_date + 30 d`, 60 days
   for pending claims). The backend keeper calls `PayoutReceiver::extend_contract_ttl()`
   daily (all four contracts to 30 days) and `extend_policy_ttl(policy_id)` for open or
   pending policies whose shortest TTL is under 30 days. An archived security flag never
   reads as absent: it fails closed or is restored with its true value.
6. **Access control.** Every state-changing fn takes an explicit `caller: Address`, calls
   `caller.require_auth()`, and checks the `microcrop_shared::roles` registry (the
   AccessControl replacement).

## Deploying with the Stellar CLI

Use `scripts/deploy.sh` and `scripts/verify-deployment.sh` (see
[`docs/DEPLOYMENT_RUNBOOK.md`](docs/DEPLOYMENT_RUNBOOK.md), including *Upgrading to the
drought path and the rent cap*). The manual flow, for reference:

```sh
# 0. Build optimised WASM
stellar contract build
stellar contract optimize --wasm target/wasm32v1-none/release/microcrop_policy_manager.wasm

# 1. Create + fund a deployer identity (testnet faucet)
stellar keys generate deployer --network testnet --fund

# 2. Deploy each contract. All four take a __constructor, so pass its args after `--`.
#    (Ref: https://developers.stellar.org/docs/build/smart-contracts/getting-started/deploy-to-testnet)

# PolicyNFT  __constructor(admin, name, symbol)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_policy_nft.wasm \
  --source-account deployer --network testnet --alias policy_nft \
  -- --admin <ADMIN_G...> --name "MicroCrop Policy" --symbol "MCP"

# PolicyManager  __constructor(admin)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_policy_manager.wasm \
  --source-account deployer --network testnet --alias policy_manager \
  -- --admin <ADMIN_G...>

# Treasury  __constructor(usdc, backend_wallet, admin)
#   usdc = the Stellar Asset Contract (SEP-41) address of USDC on the target network.
#   The constructor panics (UsdcDecimalsMismatch, #217) unless usdc.decimals() == 7.
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_treasury.wasm \
  --source-account deployer --network testnet --alias treasury \
  -- --usdc <USDC_SAC_C...> --backend_wallet <G...> --admin <ADMIN_G...>

# PayoutReceiver  __constructor(treasury, policy_manager, admin)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_payout_receiver.wasm \
  --source-account deployer --network testnet --alias payout_receiver \
  -- --treasury <TREASURY_C...> --policy_manager <PM_C...> --admin <ADMIN_G...>

# 3. Wire the contracts together + grant roles (examples; admin must auth)
stellar contract invoke --id policy_manager --source-account admin --network testnet \
  -- set_policy_nft --caller <ADMIN_G...> --policy_nft <NFT_C...>
stellar contract invoke --id policy_nft --source-account admin --network testnet \
  -- set_policy_manager --caller <ADMIN_G...> --policy_manager <PM_C...>
stellar contract invoke --id treasury --source-account admin --network testnet \
  -- set_policy_manager --caller <ADMIN_G...> --pm <PM_C...>
stellar contract invoke --id treasury --source-account admin --network testnet \
  -- set_payout_receiver --caller <ADMIN_G...> --pr <PR_C...>

# 4. Configure the PayoutReceiver signer + network domain (MUST match the Lit PKP signer)
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  -- set_authorized_signer --caller <ADMIN_G...> --pubkey <65-byte SEC-1 uncompressed hex>
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  -- set_network_domain --caller <ADMIN_G...> --domain <32-byte hex>

# 5. Grant operational roles (Backend, Oracle, Payout, Relayer, Minter) via grant_role.

# 6. Enable CROP_DROUGHT (MethodologyKind 1) and run the keeper once.
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  -- set_methodology_enabled --caller <ADMIN_G...> --kind 1 --enabled true
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  --send=yes -- extend_contract_ttl   # --send=yes: the CLI never submits a TTL-only call otherwise
```

For `upgrade`: publish the new WASM (`stellar contract upload --wasm <new>.wasm` returns a
hash), then call each contract's `upgrade(caller, new_wasm_hash)` from the `Upgrader` role.
`PolicyNft` is intentionally **not** upgradeable (the Solidity `PolicyNFT` is plain
AccessControl, not UUPS).

See the deferred items in [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) before any
mainnet deploy.
