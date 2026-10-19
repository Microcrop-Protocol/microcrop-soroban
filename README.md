# microcrop-soroban

A Soroban (Stellar smart-contract) **preparation port** of the MicroCrop parametric
crop-insurance protocol: a Rust/WASM re-implementation of the EVM contracts in
`microcrop-contracts/microcrop/src/`, with the custodial Treasury replaced by a
non-custodial **SettlementAdapter** (Option B).

> **2026-10-19 — Option B: no custody.** The per-org USDC Treasury is gone. After the
> PayoutReceiver verifies a signed determination it records an **obligation** in the
> `SettlementAdapter` (`Owed`); the backend, holding the `Funder` role, then
> attests off-chain funding and settlement (`Funded`, `Settled`) with a reference hash.
> No contract holds tokens or receives premiums; premium proof stays off-chain (the M-Pesa
> reference). `Funded` / `Settled` are attestations, not proof of payment. This is
> deliverable 2 of the Stellar Instaward SOW and the company's 2026-09-09 non-custodial
> decision. See [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) §11.

> **Status: preparation build, not a migration.** All four contracts compile to WASM,
> the full domain logic is implemented, and each crate ships a unit-test suite that
> passes. Treasury-era testnet deployments exist; Option B has not been deployed yet. The
> contracts have **not** been audited on Soroban, deployed to mainnet, or wired to the
> off-chain stack (Lit PKP signer, backend, indexer). See [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) for the EVM→Soroban
> mapping and the honest DEFERRED list.

> **2026-10-18 — drought path and rent cap.** PayoutReceiver settles CROP_DROUGHT
> determinations (`crop-drought-rdi-1.0`, the methodology the production signer signs) via
> `submit_drought_determination`; `create_policy` sells `Drought` cover only; the retired
> CROP_DAMAGE path is behind a methodology switch, off by default. Storage rent is capped:
> ordinary calls no longer prepay months of code rent, and a permissionless keeper
> (`extend_contract_ttl`) keeps the contracts alive. The Lit drought action still has to sign
> the Soroban preimage before anything settles (MIGRATION_NOTES §5, §9, §10).

> **Target architecture (2026-08-21, first step done 2026-10-19):** carrier statutory
> reserves stay on the carrier's books; only a bounded payout facility funds verified
> obligations. Option B ships the obligation layer as a minimal `SettlementAdapter`; the
> PayoutReceiver's adapter is admin-settable (`set_settlement_adapter`, bound by a
> handshake), so a real `FundingFacility` can replace the stub later without redeploying
> the PayoutReceiver. Logos Storage is planned for encrypted evidence bundles, with
> CID/hash commitments recorded in the obligation layer.

## Contract overview

| Solidity source (v)        | Soroban crate              | Contract struct  | Role |
| -------------------------- | -------------------------- | ---------------- | ---- |
| `PolicyManager.sol` v3.1.0 | `crates/policy-manager`    | `PolicyManager`  | Policy lifecycle (create → activate → claim/cancel/expire), per-farmer caps, per-org outstanding-exposure accounting. |
| (new, Option B)            | `crates/settlement-adapter` | `SettlementAdapter` | **Non-custodial obligation ledger.** One obligation per (PayoutReceiver, policy), recorded only by the bound PayoutReceiver; `Owed → Funded → Settled` attestations by the `Funder` role only (never the org that owes it), each with a reference hash; role grants emit events. Holds no tokens, makes no calls. |
| `PayoutReceiver.sol` v2.1.0 + v2.2.0 drought path | `crates/payout-receiver`   | `PayoutReceiver` | **Security crux.** Verifies a PKP-signed parametric determination (CROP_DROUGHT; CROP_DAMAGE retired), recomputes the index and re-derives the payout on-chain, triple replay guard, then records the obligation in the SettlementAdapter (return value checked). |
| `PolicyNFT.sol`            | `crates/policy-nft`        | `PolicyNft`      | Soulbound-while-active policy certificate + ownership/distributor registry (`tokenId == policyId`). |
| shared types/errors/roles  | `crates/shared` (`microcrop_shared`) | — | `#[contracttype]` structs, `#[contracterror]` codes, the `roles` access-control registry, cross-contract `#[contractclient]` traits, the **frozen** determination byte encoders (crop and drought), the rdi-1.0 index recompute, and the TTL (rent) policy. |
| (testnet only)             | `crates/testnet-policy-fixture` | `TestnetPolicyFixture` | PolicyManager stand-in that injects policy state for `scripts/testnet-evidence.sh`. Not a sale; refuses mainnet; never built by default or deployed by `deploy.sh`. |

The `Treasury` crate (`Treasury.sol` v3.1.0 port: USDC custody, per-org reserves, pending
payouts) was removed on 2026-10-19; its error range 200..=299 stays retired.

Every contract depends on `microcrop-shared`. The shared crate's `interfaces.rs`,
`encoding.rs`, `types.rs` and `errors.rs` are the frozen cross-contract contract — the
byte layout in `encoding.rs` in particular MUST match the off-chain Lit PKP signer.

## Toolchain (verified against live Stellar/crates.io docs, 2026-07-23)

- **soroban-sdk `=27.0.2`** — current stable on crates.io, published 2026-07-23
  (confirmed: <https://crates.io/crates/soroban-sdk>, <https://docs.rs/soroban-sdk/27.0.2/>).
- **Rust 1.91.1** — pinned in `rust-toolchain.toml` (auto-installed by rustup, with rustfmt
  and clippy). The SDK MSRV is 1.91.0, but 1.91.0 has a known wasm miscompile and
  `stellar contract build` refuses it, so the release workflow could not build with it.
- **`Cargo.lock` is committed** so a verified build in CI resolves the same dependencies as
  every other build of that commit.
- **Target `wasm32v1-none`** — the current Soroban WASM target. `wasm32-unknown-unknown`
  is **rejected** by soroban-sdk 27.x on Rust ≥ 1.82 (it enables `reference-types` /
  `multi-value` the Soroban host does not support). Use `wasm32v1-none`.
- **Stellar CLI ≥ 22** for `stellar contract build|optimize|deploy|invoke`
  (install: `cargo install --locked stellar-cli` or `brew install stellar-cli`). CI and the
  release build use **25.1.0**, the version the pinned release workflow installs. CLI
  versions stamp different metadata, so the same source gives a different hash per CLI.

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
#      target/wasm32v1-none/release/microcrop_settlement_adapter.wasm
#      target/wasm32v1-none/release/microcrop_payout_receiver.wasm
#      target/wasm32v1-none/release/microcrop_policy_nft.wasm
# (the testnet fixture is not a default member: build it with -p microcrop-testnet-policy-fixture)

# ...or, with the Stellar CLI (builds every crate + can optimise):
stellar contract build
stellar contract optimize --wasm target/wasm32v1-none/release/microcrop_settlement_adapter.wasm
```

> **Note on the requested command.** `cargo build --target wasm32-unknown-unknown
> --release` is documented above as the "4 contracts to WASM" step, but soroban-sdk
> 27.0.2 hard-errors on that legacy target (see build.rs panic: *"use 'wasm32v1-none'"*).
> The equivalent supported command is `cargo build --target wasm32v1-none --release`,
> which produces all four `.wasm` files. This is the target `rust-toolchain.toml` pins.

## Test

```sh
cargo test                              # all crates
cargo test -p microcrop-settlement-adapter   # a single contract (note: package name, not path)
cargo build --target wasm32v1-none --release && \
  cargo test -p microcrop-integration-tests -- --ignored   # the real-WASM lane
```

Package names (for `-p`): `microcrop-shared`, `microcrop-policy-manager`,
`microcrop-settlement-adapter`, `microcrop-payout-receiver`, `microcrop-policy-nft`,
`microcrop-integration-tests`, `microcrop-testnet-policy-fixture`.

## Current build & test status (2026-10-19)

```
stellar contract build --optimize (stellar-cli 25.1.0, Rust 1.91.1)  ->  4/4 contracts
                                       built      optimized
    microcrop_policy_manager.wasm      41,859 B   38,786 B
    microcrop_settlement_adapter.wasm  34,248 B   32,090 B
    microcrop_payout_receiver.wasm     51,876 B   47,503 B
    microcrop_policy_nft.wasm          31,903 B   30,221 B
  (testnet only, never released) microcrop_testnet_policy_fixture.wasm  23,447 B  22,469 B
  (plain `cargo build --target wasm32v1-none --release` gives larger files, 47-76 KB.
   Deploy the CLI-built release assets; the sizes above are those.)

cargo test --workspace                         ->  202 passed, 0 failed, 1 ignored
    microcrop-payout-receiver     80 passed   (crop + drought: every check in order, nothing recorded on any rejection, extreme coordinates -> 321 not a trap, real secp256k1 round-trip, obligation fields + return-value check (329), unbound adapter, set_settlement_adapter handshake (328), replay after an adapter swap, keeper resilience, archived-flag safety, rent cap, upgrade gate)
    microcrop-policy-manager      45 passed   (lifecycle, ACTIVE-only claim, caps, claim window, exposure, roles, plot coords, Drought-only sale with a 117-day floor, TTL targets, upgrade gate)
    microcrop-settlement-adapter  25 passed   (Funder-only marks (the org is refused), exact auth trees, to_xdr events incl. role grants, idempotent/conflicting records, receiver rotation, state machine, pagination, TTL, archived entries, no-custody structural check, proptest)
    microcrop-shared              19 passed   (drought golden vectors, EVM + JS cross-checks, rdi-1.0 grid, TTL helpers, ABI pin of every error code and ordinal)
    microcrop-integration-tests   16 passed, 1 ignored   (four real contracts on one Env, no token: determination -> Owed -> Funded -> Settled, replay incl. after an adapter swap, real 2022 ERA5 season, keepers; ignored: the real-WASM lane incl. a happy-path upgrade of adapter, PayoutReceiver and PolicyManager, passes with --ignored after a wasm build, and runs in CI)
    microcrop-policy-nft          14 passed   (mint, soulbound transfer, PM-only status, paging, TTL targets)
    microcrop-testnet-policy-fixture 3 passed (mainnet refusal, Oracle-gated writes)

cargo clippy --workspace --all-targets -- -D warnings  ->  clean (0 warnings)
cargo fmt --all -- --check                     ->  clean
```

## CI and verified builds

- **`.github/workflows/ci.yml`** (every PR, every push to `master`, read-only token): fmt,
  clippy with `-D warnings`, `cargo test --workspace`, `stellar contract build --optimize` of
  every contract crate with the release CLI, a check that each WASM carries
  `source_repo=github:Microcrop-Protocol/microcrop-soroban`, a size report in the job
  summary, and the `#[ignore]`d real-WASM lane against those binaries. CI binaries are never
  deploy inputs.
- **`.github/workflows/release.yml`** (on a `vX.Y.Z` tag of a commit on `master`): a
  [SEP-55](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0055.md)
  verified build of each of the four contracts through
  `stellar-expert/soroban-build-workflow`, pinned to a reviewed commit. Each contract gets
  a GitHub release with its WASM and a build attestation over it; stellar.expert and Stellar
  Lab show the source commit for the deployed hash.
- **Deploy the release, not a local build:** `./scripts/deploy.sh --network mainnet
  --release vX.Y.Z` verifies every asset's attestation, and that all four come from the
  one commit the tag points at, before deploying it; mainnet refuses anything else.
  `./scripts/verify-release.sh vX.Y.Z` runs the same checks on their own (use it before an
  `upgrade`).

A verified build proves which commit a workflow run built the binary from. It does not prove
the code is safe; these contracts are unaudited. The pin covers the reusable workflow file,
not the actions it calls, which are on mutable tags. That workflow creates each release
before attesting it and tags it at `master`'s head, so the attestation's source commit, not
the release's tag or archive, is the authoritative one. Tag and `master` rulesets are not set
yet (runbook, *Protect the tags and master*). Steps, verification commands and the
residual risk: [`docs/DEPLOYMENT_RUNBOOK.md`](docs/DEPLOYMENT_RUNBOOK.md), *Cutting a release*.

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
   `PolicyPaid(policy_id)` (in the PayoutReceiver, so swapping the adapter never reopens a
   paid policy) + the adapter's one obligation per (PayoutReceiver, policy).
4. **No custody; one obligation per (PayoutReceiver, policy).** No contract holds or moves a
   token: the adapter imports no token interface and makes no cross-contract calls (a test
   reads its source to keep it so). Only the bound PayoutReceiver, with its own
   authorization, can record an obligation; an identical repeat is a no-op, anything else
   for the same (PayoutReceiver, policy) is refused (502). Status only moves forward
   (`Owed → Funded → Settled`), each mark by a `Funder` only (the org owes the money, so it cannot mark its own), with a
   non-zero reference hash that cannot be overwritten. The PayoutReceiver checks the
   returned obligation field by field (329). Amounts are USDC base units at **7 decimals**,
   as a unit of account.
5. **Storage rent is capped.** Ordinary calls extend instance + code only below 3 days, to
   10 days; data entries live as long as the policy needs them (`end_date + 30 d`; open
   obligations below 30 days to 60 days). The backend keeper calls
   `PayoutReceiver::extend_contract_ttl()` daily (all four contracts, the adapter included,
   to 30 days) and `extend_policy_ttl(policy_id)` for open policies and for policies with an
   Owed or Funded obligation whose shortest TTL is under 30 days. An archived security flag
   or obligation never reads as absent: it fails closed or is restored with its true value.
6. **Access control.** Every state-changing fn takes an explicit `caller: Address`, calls
   `caller.require_auth()`, and checks the `microcrop_shared::roles` registry (the
   AccessControl replacement).

## Deploying with the Stellar CLI

Use `scripts/deploy.sh` and `scripts/verify-deployment.sh` (see
[`docs/DEPLOYMENT_RUNBOOK.md`](docs/DEPLOYMENT_RUNBOOK.md)); `scripts/testnet-evidence.sh`
produces the testnet evidence (MIGRATION_NOTES §11). A Treasury-era deployment cannot be
upgraded in place: deploy fresh. Mainnet takes only the attested release WASM
(`deploy.sh --release vX.Y.Z`, see *CI and verified builds*). The manual flow, for testnet
reference:

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
  --wasm target/wasm32v1-none/release/microcrop_policy_nft.optimized.wasm \
  --source-account deployer --network testnet --alias policy_nft \
  -- --admin <ADMIN_G...> --name "MicroCrop Policy" --symbol "MCP"

# PolicyManager  __constructor(admin)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_policy_manager.optimized.wasm \
  --source-account deployer --network testnet --alias policy_manager \
  -- --admin <ADMIN_G...>

# SettlementAdapter  __constructor(admin)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_settlement_adapter.optimized.wasm \
  --source-account deployer --network testnet --alias settlement_adapter \
  -- --admin <ADMIN_G...>

# PayoutReceiver  __constructor(settlement_adapter, policy_manager, admin)
stellar contract deploy \
  --wasm target/wasm32v1-none/release/microcrop_payout_receiver.optimized.wasm \
  --source-account deployer --network testnet --alias payout_receiver \
  -- --settlement_adapter <ADAPTER_C...> --policy_manager <PM_C...> --admin <ADMIN_G...>

# 3. Wire the contracts together (admin must auth). The adapter MUST be bound back to the
#    PayoutReceiver, or every determination reverts (NotPayoutReceiver, 500).
stellar contract invoke --id policy_manager --source-account admin --network testnet \
  -- set_policy_nft --caller <ADMIN_G...> --policy_nft <NFT_C...>
stellar contract invoke --id policy_nft --source-account admin --network testnet \
  -- set_policy_manager --caller <ADMIN_G...> --policy_manager <PM_C...>
stellar contract invoke --id settlement_adapter --source-account admin --network testnet \
  -- set_payout_receiver --caller <ADMIN_G...> --payout_receiver <PR_C...>

# 4. Configure the PayoutReceiver signer + network domain (MUST match the Lit PKP signer)
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  -- set_authorized_signer --caller <ADMIN_G...> --pubkey <65-byte SEC-1 uncompressed hex>
stellar contract invoke --id payout_receiver --source-account admin --network testnet \
  -- set_network_domain --caller <ADMIN_G...> --domain <32-byte hex>

# 5. Grant operational roles via grant_role (Role numbers: Backend=1 Oracle=2 Relayer=4
#    Minter=6 Funder=7): Backend on PM, Minter on NFT -> PM, Oracle on PM -> PR, Relayer on
#    PR, Funder on the adapter -> the backend.

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
