# microcrop-soroban

MicroCrop's parametric drought-cover contracts for Stellar, written in Rust for Soroban.

MicroCrop is parametric crop and livestock insurance infrastructure for African smallholders, sold B2B. Partner organisations — insurers, reinsurers, cooperatives, microfinance institutions and agtech platforms — underwrite the cover and hold the relationship with the farmer. MicroCrop operates the platform and acts as the calculating agent: it computes and verifies the determination, and records the resulting payout obligation. It does not underwrite, and it does not hold partner reserves.

These contracts are the Stellar record of that work: the policy a partner has written, the signed determination that a season failed, and the obligation that follows from it. They are a Rust re-implementation of the Solidity protocol in `microcrop-contracts`, with one deliberate departure. No contract in this repository holds or moves a token. The per-organisation USDC Treasury of the Solidity design has been removed and replaced by a `SettlementAdapter` that records obligations only. On Stellar, MicroCrop handles no funds: the partner collects the premium and pays the farmer off-platform in local currency, and the chain carries a hash identifying the partner's report of that payment.

## Status

| | |
| --- | --- |
| Release | `v0.1.0`, built by the SEP-55 verified-build workflow from commit `d7051b1` |
| Testnet | Deployed, wired and verified on 2026-10-22 (`scripts/verify-deployment.sh`, 31 of 31 checks) |
| Mainnet | Not deployed |
| Audit | No third-party audit exists. An in-house review in 2026-10 raised 11 issues, all since fixed; that is not a substitute for an independent audit. |
| Oracle | The live `PayoutReceiver` accepts the production Lit PKP only. The Lit drought action signs the Soroban preimage (`microcrop-lit-oracle`, merged 2026-10-18), but no PKP-signed determination has been submitted on-chain. |
| Backend | The Stellar writer, settlement worker and keeper are merged in `microcrop-core` behind `STELLAR_SETTLEMENT_ENABLED`, which defaults to off, and are not configured against these addresses. The testnet run was driven from the CLI. |

What the testnet run establishes is that the port works on Stellar. It is not evidence that the settlement model is in production, and these contracts are not production-ready. The run is documented transaction by transaction in [`docs/evidence/INSTAWARD_TESTNET_EVIDENCE.md`](docs/evidence/INSTAWARD_TESTNET_EVIDENCE.md); the open items are listed in [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) §5.

## Contracts

| Crate | Contract | Responsibility |
| --- | --- | --- |
| `crates/policy-manager` | `PolicyManager` | Policy lifecycle (create, activate, claim, cancel, expire), per-farmer caps and per-organisation outstanding exposure. `create_policy` accepts drought cover only. |
| `crates/payout-receiver` | `PayoutReceiver` | Verifies a PKP-signed determination, recomputes the index and re-derives the payout on-chain, enforces the replay guards, and records the obligation. The security crux of the system. |
| `crates/settlement-adapter` | `SettlementAdapter` | The obligation ledger: one obligation per (PayoutReceiver, policy), `Owed → Funded → Settled`. Holds no tokens and makes no cross-contract calls. |
| `crates/policy-nft` | `PolicyNft` | Policy certificate and ownership registry, soulbound while the policy is active (`token_id == policy_id`). |
| `crates/shared` | — | Types, error codes, the role registry, cross-contract interfaces, the frozen determination encoders, the rdi-1.0 index recompute and the storage-rent policy. |
| `crates/integration-tests` | — | All four contracts on one host environment, plus an `#[ignore]`d lane against the built WASM. |
| `crates/testnet-policy-fixture` | `TestnetPolicyFixture` | Testnet-only `PolicyManager` stand-in for the evidence script. Refuses to deploy on mainnet; not a default workspace member and never deployed by `deploy.sh`. |

The byte layout in `crates/shared/src/encoding.rs` is the frozen cross-contract interface and must match the Lit signer exactly; it changes only alongside the oracle and its golden vectors. The live methodology is `crop-drought-rdi-1.0`. The retired `crop-dualindex-1.0` crop path remains behind an admin switch, disabled by default.

## Settlement model

```
submit_drought_determination
  → signature recovered and compared to the authorised PKP key
  → index recomputed from the signed rainfall evidence, payout re-derived
  → policy, plot coordinates, season window, freshness and replay guards checked
  → SettlementAdapter.record_obligation(...)  → Owed
off-chain  the partner pays the farmer in local currency and reports it to MicroCrop
  → mark_funded(funder, id, sha256(partner report reference))
  → mark_settled(funder, id, sha256(partner report reference))
```

- The signed determination is the sole payout authority. The `Relayer` role is an anti-spam gate, nothing more.
- The payout is re-derived on-chain from the signed evidence. The oracle's arithmetic is checked, not trusted.
- A policy can be paid once. Three guards enforce it: the consumed-determination digest, the `PolicyPaid` flag in the `PayoutReceiver`, and the adapter's one obligation per (PayoutReceiver, policy).
- `Funded` and `Settled` are attestations by the `Funder` role, held by the MicroCrop backend. The partner pays the farmer off-platform and reports it; the backend marks the obligation and records a hash identifying that report. Nothing on-chain proves that money moved, and the organisation that owes the obligation cannot mark its own.
- Amounts are recorded in USDC base units at 7 decimals as a unit of account. No token is transferred, and the farmer never touches crypto.

Rationale, error codes and the full state machine: [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) §11.

## Testnet deployment (v0.1.0)

| Contract | Address |
| --- | --- |
| `PolicyNft` | [`CB6FEFIXBDID45GVVCDWMLNKFBIWIJEDVFPIUJDAMRIISYWEA5FQ3HV4`](https://stellar.expert/explorer/testnet/contract/CB6FEFIXBDID45GVVCDWMLNKFBIWIJEDVFPIUJDAMRIISYWEA5FQ3HV4) |
| `PolicyManager` | [`CDGQWA6QC2BRR5HHFJVNWAY4C34WXH7BCUFGHRDB4KT4WDMVUMDGDBSR`](https://stellar.expert/explorer/testnet/contract/CDGQWA6QC2BRR5HHFJVNWAY4C34WXH7BCUFGHRDB4KT4WDMVUMDGDBSR) |
| `SettlementAdapter` | [`CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5`](https://stellar.expert/explorer/testnet/contract/CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5) |
| `PayoutReceiver` | [`CCBWJMQM42WA6AWYEBAJCP6JCJBDPB7TBWAAEEECIN4OOTZGRAELBVX3`](https://stellar.expert/explorer/testnet/contract/CCBWJMQM42WA6AWYEBAJCP6JCJBDPB7TBWAAEEECIN4OOTZGRAELBVX3) |

WASM hashes and the source commit for each contract are recorded in the evidence document.

## Toolchain

| | |
| --- | --- |
| Rust | 1.91.1, pinned in `rust-toolchain.toml` with the `wasm32v1-none` target. 1.91.0 has a WASM miscompile and is rejected by the Stellar CLI. |
| soroban-sdk | `=27.0.2` |
| Target | `wasm32v1-none`. soroban-sdk 27 rejects `wasm32-unknown-unknown`. |
| Stellar CLI | 25.1.0 in CI and in the release workflow. Other versions build, but stamp different metadata and so produce different hashes. |
| Lockfile | `Cargo.lock` is committed, so a verified build in CI resolves what every other build of that commit resolves. |

## Build and test

```sh
cargo test --workspace                                   # unit and integration tests
cargo test -p microcrop-payout-receiver                  # one crate, by package name
stellar contract build --optimize                        # the four deployable contracts
cargo test -p microcrop-integration-tests -- --ignored   # the real-WASM lane, after a build
```

CI runs `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` on every pull request, alongside the tests, an optimised build of each contract and a check of the `source_repo` metadata in each WASM. CI binaries are never deploy inputs.

On macOS without an accepted Xcode licence, prefix commands with `DEVELOPER_DIR=/Library/Developer/CommandLineTools`.

## Deployment

The four contracts are interdependent: `PayoutReceiver` takes the adapter and `PolicyManager` addresses in its constructor, the adapter must be bound back to the `PayoutReceiver`, and five cross-contract role grants must follow. A missed step fails on the first claim rather than at deploy time, so deployment is scripted rather than documented as a checklist. `scripts/deploy.sh` is idempotent and resumable, records each step in a state file, and refuses to mix builds.

```sh
ADMIN_KEY=… BACKEND_ADDR=G… RELAYER_ADDR=G… AUTHORIZED_SIGNER_PUBKEY=04… \
  ./scripts/deploy.sh --network testnet --release v0.1.0 --state-dir deployments/v0.1.0
BACKEND_ADDR=… RELAYER_ADDR=… AUTHORIZED_SIGNER_PUBKEY=04… \
  ./scripts/verify-deployment.sh --network testnet --release v0.1.0 --state-dir deployments/v0.1.0
```

Mainnet accepts attested release assets only (`--release vX.Y.Z`); a local build is refused. `PolicyNft` is not upgradeable, matching the Solidity contract; the other three are upgraded by the `Upgrader` role against a published WASM hash. A Treasury-era deployment cannot be upgraded in place and must be redeployed. Environment variables, the network domain, the daily keeper call and the upgrade procedure are in [`docs/DEPLOYMENT_RUNBOOK.md`](docs/DEPLOYMENT_RUNBOOK.md).

## Releases

A `vX.Y.Z` tag on `master` triggers `.github/workflows/release.yml`, which builds each contract through the [SEP-55](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0055.md) verified-build workflow and publishes a per-contract GitHub release carrying the WASM and a build attestation. `scripts/verify-release.sh vX.Y.Z` re-runs those checks independently before a deploy or an upgrade.

A verified build establishes which commit produced a binary. It does not establish that the code is correct or safe. The limits of the pin, and the residual risk in it, are set out in the runbook under *Cutting a release*.

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/MIGRATION_NOTES.md`](docs/MIGRATION_NOTES.md) | The Solidity-to-Soroban mapping, platform-forced changes, the drought path, the rent cap, the removal of custody, and the deferred list. |
| [`docs/DEPLOYMENT_RUNBOOK.md`](docs/DEPLOYMENT_RUNBOOK.md) | Deploying, verifying, releasing, upgrading and operating the contracts. |
| [`docs/evidence/INSTAWARD_TESTNET_EVIDENCE.md`](docs/evidence/INSTAWARD_TESTNET_EVIDENCE.md) | The v0.1.0 testnet run against the Instaward SOW deliverables. |
| `microcrop-contracts/microcrop/src/` | The Solidity contracts this port derives from. |
| `microcrop-context/docs/` | Cross-repo product, architecture and decision records. |
