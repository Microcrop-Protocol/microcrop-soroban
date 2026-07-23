# microcrop-soroban

A Soroban (Stellar smart-contract) **preparation port** of the MicroCrop parametric
crop-insurance protocol — a faithful Rust/WASM re-implementation of the four EVM
contracts in `microcrop-contracts/microcrop/src/`.

> **Status: preparation build, not a migration.** All four contracts compile to WASM,
> the full domain logic is implemented, and each crate ships a unit-test suite that
> passes. This is a clean, building, tested *first version*. It has **not** been
> deployed, audited on Soroban, or wired to the off-chain stack (Lit PKP signer,
> backend, indexer). See [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) for the
> EVM→Soroban mapping and the honest DEFERRED list.

## Contract overview

| Solidity source (v)        | Soroban crate              | Contract struct  | Role |
| -------------------------- | -------------------------- | ---------------- | ---- |
| `PolicyManager.sol` v3.1.0 | `crates/policy-manager`    | `PolicyManager`  | Policy lifecycle (create → activate → claim/cancel/expire), per-farmer caps, per-org outstanding-exposure accounting. |
| `Treasury.sol` v3.1.0      | `crates/treasury`          | `Treasury`       | USDC custody, premium split (per-org fee), **per-org reserves + on-chain solvency**, payouts, fee withdrawal. |
| `PayoutReceiver.sol` v2.1.0| `crates/payout-receiver`   | `PayoutReceiver` | **Security crux.** Verifies a PKP-signed parametric determination, re-derives payout on-chain, triple replay guard, then dispatches the payout. |
| `PolicyNFT.sol`            | `crates/policy-nft`        | `PolicyNft`      | Soulbound-while-active policy certificate + ownership/distributor registry (`tokenId == policyId`). |
| shared types/errors/roles  | `crates/shared` (`microcrop_shared`) | — | `#[contracttype]` structs, `#[contracterror]` codes, the `roles` access-control registry, cross-contract `#[contractclient]` traits, and the **frozen** canonical determination byte encoder. |

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

## Current build & test status

```
cargo build --target wasm32v1-none --release   ->  4/4 WASM contracts built
    microcrop_policy_manager.wasm    ~44 KB
    microcrop_treasury.wasm          ~47 KB
    microcrop_payout_receiver.wasm   ~44 KB
    microcrop_policy_nft.wasm        ~27 KB

cargo test                                     ->  66/66 passed, 0 failed
    microcrop-payout-receiver   20 passed   (real secp256k1 sign→recover round-trip)
    microcrop-treasury          18 passed   (premium split, per-org solvency, replay)
    microcrop-policy-manager    17 passed   (lifecycle, caps, exposure, roles)
    microcrop-policy-nft        11 passed   (mint, soulbound transfer, PM-only status)
    microcrop-shared             0 tests    (types/errors only)
```

## Security-critical invariants (preserved from the Solidity)

1. **Determination signature is the sole payout authority.** `submit_determination`
   reconstructs the digest on-chain via `shared::encoding::determination_digest`, calls
   `env.crypto().secp256k1_recover`, and byte-compares the recovered SEC-1 65-byte key to
   the stored `authorized_signer`. The `Relayer` role is anti-spam only.
2. **Payout math re-derived on-chain.** `payout == sum_insured * damage_bp / 10_000`
   (overflow-trapping i128), plus the weighted-damage invariant `60*w + 40*s == bp`
   (satellite-only: `100*s == bp`), threshold `bp ≥ 3000`, freshness `MAX_REPORT_AGE = 3600`.
   The signed `payout_amount` is verified against the re-derived value, never trusted.
3. **Triple replay guard.** temporary `ConsumedDetermination(digest)` + persistent
   `PolicyPaid(policy_id)` + Treasury `PayoutProcessed(policy_id)`.
4. **Per-org solvency.** Payouts debit only the policy's org reserve
   (`InsufficientOrgReserve` if short — loud, never silent); `reserve_required =
   outstanding_sum_insured * ratio_bps / 10_000`; withdrawals are surplus-only; the
   invariant `usdc.balance(this) >= Σ org_reserve + accumulated_fees` holds after every op.
5. **Access control.** Every state-changing fn takes an explicit `caller: Address`, calls
   `caller.require_auth()`, and checks the `microcrop_shared::roles` registry (the
   AccessControl replacement).

## Deploying later with the Stellar CLI

This project is **not deployed yet.** When it is time (testnet first), the flow is:

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
#   usdc = the Stellar Asset Contract (SEP-41) address of USDC on the target network
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
```

For `upgrade`: publish the new WASM (`stellar contract upload --wasm <new>.wasm` returns a
hash), then call each contract's `upgrade(caller, new_wasm_hash)` from the `Upgrader` role.
`PolicyNft` is intentionally **not** upgradeable (the Solidity `PolicyNFT` is plain
AccessControl, not UUPS).

See the deferred items in [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) before any
mainnet deploy.
