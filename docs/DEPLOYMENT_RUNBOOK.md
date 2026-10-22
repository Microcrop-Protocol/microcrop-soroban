# Deployment runbook — MicroCrop on Soroban

Deploys, wires and configures all four contracts (Option B, 2026-10-19): PolicyNft,
PolicyManager, SettlementAdapter and PayoutReceiver. There is no Treasury and no USDC token:
a verified determination becomes an obligation in the SettlementAdapter, funded and settled
off-chain. Two scripts do the work:

```bash
./scripts/deploy.sh --network testnet --dry-run   # print every call, change nothing
./scripts/deploy.sh --network testnet             # do it
./scripts/verify-deployment.sh --network testnet  # read the result back

# mainnet: only the SEP-55 verified build of a release tag (see "Cutting a release")
./scripts/deploy.sh --network mainnet --release v0.2.0
./scripts/verify-deployment.sh --network mainnet --release v0.2.0
```

---

## Why this is a script and not a checklist

The contracts are not independent. PayoutReceiver's constructor consumes the
SettlementAdapter and PolicyManager addresses, so deployment has a required order, and the
adapter must then be bound back to the PayoutReceiver. Worse, **five role grants and three
cross-references have to be made after deployment, and none of them fail at deploy time.** A
missed step produces a deployment that looks complete and is dead on the claim path — you
find out on the first real claim, on chain, with a farmer waiting.

The sequence in `deploy.sh` is lifted from `crates/integration-tests`, which exercises the
identical wiring against all four real contracts. It is verified, not remembered.

---

## Prerequisites

| Thing | Notes |
|---|---|
| `stellar` CLI | `cargo install --locked stellar-cli` |
| `jq` | `brew install jq` |
| `gh` CLI, logged in | `brew install gh && gh auth login`. Needed for `--release`: it downloads the release assets and verifies their build attestations |
| The wasm | **Mainnet:** a release tag (`--release vX.Y.Z`); nothing to build. **Testnet:** either a release tag or a local build, `cargo build --release --target wasm32v1-none` (4 artifacts; the script optimizes them). The testnet fixture is not a default member and is never built, released or deployed by `deploy.sh` |
| Funded admin identity | `stellar keys generate deployer --network testnet --fund` |

### Environment

```bash
export ADMIN_KEY=deployer              # stellar CLI identity; holds Admin+Upgrader on all four
export BACKEND_ADDR=G...               # backend signs policy calls with this
export RELAYER_ADDR=G...               # allowed to submit determinations (anti-spam gate)
export AUTHORIZED_SIGNER_PUBKEY=04...  # 65-byte SEC-1 pubkey of the Lit PKP — 130 hex chars
export FUNDER_ADDR=G...                # optional, default BACKEND_ADDR: may mark ANY obligation
                                       # funded / settled in the SettlementAdapter
```

`USDC_SAC` and `BACKEND_WALLET` are gone: no contract holds or moves a token, so there is no
`decimals()` preflight either. A state file that records a `treasury` belongs to a
Treasury-era deployment; `deploy.sh` refuses it (`--state-dir` for a fresh one).

> **`FUNDER_ADDR` attests, it does not pay.** The Funder can mark any org's obligation
> `Funded` or `Settled`, and it is the only one who can: the org owes the obligation, so a
> mark by the org would be a self-waive and the adapter refuses it (#1). Grants and
> revocations of the role emit `RoleGranted` / `RoleRevoked`. Those marks are attestations backed
> by a reference hash of the off-chain payment record, not proof that money moved
> (MIGRATION_NOTES §11).

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
3. `SettlementAdapter(admin)` — grants Admin + Upgrader; no PayoutReceiver yet
4. `PayoutReceiver(settlement_adapter, policy_manager, admin)`

**Wire cross-references**

5. `PolicyManager.set_policy_nft(nft)`
6. `PolicyNft.set_policy_manager(pm)`
7. `SettlementAdapter.set_payout_receiver(pr)` — the binding. Until it runs, every
   determination reverts `NotPayoutReceiver` (500) or `NotConfigured` (3): fail closed, but
   nothing settles.

**Grant roles** — the silent-failure section. Role numbers: Admin=0 Backend=1 Oracle=2
(3 retired) Relayer=4 Upgrader=5 Minter=6 Funder=7.

| # | Grant | Without it |
|---|---|---|
| 8 | `PolicyManager` → Backend (1) → backend | backend cannot create policies |
| 9 | `PolicyNft` → Minter (6) → **PolicyManager** | activation cannot mint a certificate |
| 10 | `PolicyManager` → Oracle (2) → **PayoutReceiver** | determinations cannot mark a policy claimed |
| 11 | `PayoutReceiver` → Relayer (4) → relayer | determinations cannot be submitted |
| 12 | `SettlementAdapter` → Funder (7) → funder | **obligations can never be marked funded/settled** (backend orgs are keyless) |

Note 9 and 10 grant roles to *contracts*, not people. That is the part most often missed.

**Configure the payout authority**

13. `PayoutReceiver.set_authorized_signer(pubkey)`
14. `PayoutReceiver.set_network_domain(domain)`
15. `PayoutReceiver.set_methodology_enabled(kind = 1, true)` — CROP_DROUGHT. Absent means
    disabled, so nothing settles until this runs. Kind 0 (CROP_DAMAGE) is retired: leave it off.

**Keep the contracts alive**

16. `PayoutReceiver.extend_contract_ttl()` — extends all four contracts' instance and code
    (PayoutReceiver, the SettlementAdapter, PolicyManager, PolicyNft) to 30 days. A fresh upload sits at the network minimum (7 days on testnet) and admin calls
    extend nothing, so run it right after the deploy (`--send=yes`: the CLI does not submit a
    TTL-only call by default); the backend keeper then runs it daily (see *Storage rent: the
    keeper* below).

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

Checks, and exits non-zero on any failure:

1. all four contracts are live and reachable;
2. the cross-references are wired: PolicyManager ↔ PolicyNft, PayoutReceiver → PolicyManager,
   and the binding in **both** directions through exact views:
   `PayoutReceiver.settlement_adapter() == ADAPTER` and
   `SettlementAdapter.payout_receiver() == PR`;
3. the roles: Minter on PolicyNft held by PolicyManager, Oracle on PolicyManager held by
   PayoutReceiver, Funder on the adapter held by `FUNDER_ADDR` (default `BACKEND_ADDR`), and,
   when set, Backend on PolicyManager and Relayer on PayoutReceiver;
4. the signer and domain;
5. CROP_DROUGHT enabled, the retired CROP_DAMAGE disabled;
6. every instance and code entry has at least 23 days of TTL left;
7. each deployed instance's wasm hash equals `sha256` of the **attested release asset** for
   `--release <tag>` (default: the release `deploy.sh` recorded in the state file), after that
   asset's build attestation has been verified. Together that says the chain runs bytes a
   GitHub workflow built from the tagged commit. A deployment made from a local build is
   compared with the local `*.optimized.wasm` instead, which proves only that the chain runs
   the bytes in `target/`; on mainnet that case fails.

Set `BACKEND_ADDR`, `RELAYER_ADDR`, `FUNDER_ADDR` and `AUTHORIZED_SIGNER_PUBKEY` in the
environment to enable every check. `--state-dir` points it at a deployment made with
`deploy.sh --state-dir`.

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

`--network mainnet` requires `--release <tag>` (a local build is refused, `--dry-run`
included) and typing a confirmation phrase. See **Cutting a release** below.

**Before you do:** these contracts have had **no Soroban security audit**. The Solidity
originals were reviewed in-house only; the Rust port has not been reviewed at all. The
secp256k1 signature path, the replay guards, the PayoutReceiver ↔ SettlementAdapter binding
and the obligation state machine all need fresh review against Soroban semantics — auth
trees, TTL and archival edge cases behave differently from the EVM. A single key holds Admin
and Upgrader on every contract; with a settable adapter, Admin can redirect obligations, which
is Upgrader-equivalent power. Use a multisig or custom account, and consider a timelock on
`set_settlement_adapter`, before mainnet (MIGRATION_NOTES §11, decision D-B3).

Storage TTL follows the rent cap (see **Storage rent: the keeper** below); the restore cost
of an entry that archives anyway has not been measured.

---

## Cutting a release (SEP-55 verified build)

Nothing on chain ties a WASM hash to source code. [SEP-55](https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0055.md)
closes that gap: GitHub Actions builds the contract with its source repository stamped into
the WASM's `contractmetav0` (`source_repo=github:Microcrop-Protocol/microcrop-soroban`) and
publishes a GitHub build attestation over the binary. Anyone can then walk from the on-chain
hash to the commit and the workflow run that produced it. The repository is public, which
the attestation needs: verifiers read it unauthenticated.

### Steps

1. **Merge to `master`** with CI green (`.github/workflows/ci.yml`: fmt, clippy
   `-D warnings`, `cargo test --workspace`, `stellar contract build` of every contract crate
   with the release CLI, the source_repo check, a size report, and the real-WASM lane).
2. **Tag the merged commit** with a plain semver tag and push it:
   ```bash
   git tag -a v0.2.0 -m "v0.2.0" <merged-commit> && git push origin v0.2.0
   ```
   `.github/workflows/release.yml` runs. A guard job (read-only) refuses a tag that is not
   `vMAJOR.MINOR.PATCH[-pre]` or whose commit is not on `master`. Then one job per contract
   calls `stellar-expert/soroban-build-workflow` with `package: <crate>`.
3. **Release assets.** Each job creates its own GitHub release, named
   `v0.2.0_<crate>_pkg<crate-version>_cli25.1.0` (the `_pkg…` part is omitted when the tag
   equals the crate version), with one asset, `<crate>_v<crate-version>.wasm`, already
   optimized, plus a build attestation over that file. It also registers the build with
   stellar.expert's contract validation. Four contracts, four releases:
   `microcrop-policy-nft`, `microcrop-policy-manager`, `microcrop-settlement-adapter`,
   `microcrop-payout-receiver`. The testnet fixture is never released.
4. **Deploy those files**, never a local rebuild (a different compiler environment gives a
   different hash, which matches no attestation):
   ```bash
   ./scripts/deploy.sh --network testnet --release v0.2.0     # rehearse first
   ./scripts/deploy.sh --network mainnet --release v0.2.0
   ./scripts/verify-deployment.sh --network mainnet --release v0.2.0
   ```
   `deploy.sh` downloads the four assets to `<state-dir>/releases/v0.2.0/` and, before
   deploying anything, verifies each one:
   ```bash
   gh attestation verify <file> --repo Microcrop-Protocol/microcrop-soroban \
     --signer-workflow stellar-expert/soroban-build-workflow/.github/workflows/release.yml \
     --signer-digest e43b241f114bcf02151c30d61088506c6ad3ae0b \
     --source-ref refs/tags/v0.2.0 --deny-self-hosted-runners
   ```
   and checks its `source_repo` metadata. Any failure stops the deploy. The release is
   recorded in the state file, and a resume with a different release or a local build is
   refused. `--signer-workflow` is required: the attestation is signed by the reusable
   workflow, not by a workflow in this repository, so a bare `--repo` check rejects it.
   `--release-dir <dir>` uses assets already on disk (still verified).

   It then checks that **all four assets were built from one commit, and that the tag still
   points at it**. Each attestation only says "built from `refs/tags/v0.2.0`"; a tag that was
   deleted and re-pushed on another commit after a partial failure (see *A release that fails
   partway*) can leave a release whose four assets carry the same tag but come from two
   commits. `deploy.sh`, `verify-deployment.sh` and `verify-release.sh` all refuse that, and a
   tag that moved after the build. An attestation that names no source commit is refused too.

**To upgrade a live contract**, cut a release the same way, then verify it before uploading
anything. `upgrade` takes a hash, so nothing on the upgrade path checks the attestation for
you:

```bash
./scripts/verify-release.sh v0.2.1        # downloads to releases/v0.2.1/, verifies all four,
                                          # prints each file's sha256; exits non-zero on any failure
HASH=$(stellar contract upload --wasm releases/v0.2.1/microcrop-settlement-adapter_v<ver>.wasm \
  --source admin --network mainnet)
# then `upgrade` with $HASH, which must equal the sha256 verify-release.sh printed
```

### A release that fails partway

The reusable workflow creates the GitHub release **before** it attests the file. If a job
fails between the two (the stellar.expert registration call has a 15-second timeout and sits
between them), that contract's release exists with no attestation. `deploy.sh` refuses it
("NO valid attestation"), so nothing unsafe ships, but "Re-run failed jobs" fails too: the
release name is taken.

Recover without moving the tag:

1. Delete **only** the broken per-contract release and its tag:
   ```bash
   gh release delete v0.2.0_<crate>_..._cli25.1.0 --cleanup-tag --yes \
     --repo Microcrop-Protocol/microcrop-soroban
   ```
2. On the same workflow run, **Re-run failed jobs**. It rebuilds from the same tagged commit.
3. `./scripts/verify-release.sh v0.2.0` before deploying.

**Never delete, move or re-push the `vX.Y.Z` tag itself.** The jobs whose releases survived
would not rebuild, and the release would mix two commits. If the tagged commit itself is
wrong, cut `vX.Y.Z+1`.

### Protect the tags and master (repository settings, once)

Anyone with write access can push a `v*` tag on any master commit, and that starts a run
with `contents: write`, `id-token: write` and `attestations: write`. The guard job checks
the tag's format and that its commit is on `master`; it does not check who pushed it or
whether CI passed on it. Close that in **Settings → Rules → Rulesets**:

- a **tag** ruleset that includes `v*` and **excludes `v*_*`**: restrict creations, updates
  and deletions to the release maintainers (bypass list), and block force pushes. With
  updates and deletions restricted, a `vX.Y.Z` cannot be moved. The exclusion matters: the
  per-contract `v*_*` tags are created by the workflow's `GITHUB_TOKEN`, and a ruleset that
  covered them would fail every release. Those tags never trigger `release.yml`.
- a **branch** ruleset on `master`: require a pull request and the `CI` status checks
  (`.github/workflows/ci.yml`) to pass, and block force pushes.

Neither is set yet. They are repository settings, not files, so this branch cannot add them.

### What it proves, and what it does not

It proves that a specific GitHub Actions run of the pinned workflow built this exact binary
from this commit of this repository. It says **nothing** about whether that code is correct
or safe, or whether anyone reviewed it. These contracts have had no Soroban audit; a verified
build of them is a verified build of unaudited code.

### Residual risk in the pin

`release.yml` pins `soroban-build-workflow` to a full commit SHA (`e43b241f…`, release
v25.1.0), resolved from its latest release and read at that SHA before pinning: it checks out
the tagged commit, builds from source, and attests the same file it uploads. The pin freezes
that one file only. The actions it calls (`actions/checkout@v4`, `stellar/stellar-cli@v25.1.0`,
`actions/setup-node@v4`, `actions/attest-build-provenance@v1`) are on mutable tags, resolve at
run time with this job's `contents: write`, `id-token: write` and `attestations: write`, and
cannot be pinned by a caller. If that is unacceptable, fork the workflow into this
organisation, pin those actions to SHAs in the fork, and point `release.yml` (and
`RELEASE_SIGNER_WORKFLOW` in `scripts/lib/release.sh`) at the fork.

Two behaviours of the pinned workflow to know about, both covered by the checks above:

- **Its per-contract tags are created at `master`'s head, not the built commit.** It runs
  `gh release create` without `--target`, so each `vX.Y.Z_<crate>_…` tag, and the release's
  "Source code" archive, point at whatever `master` was when the job ran. The build itself
  checks out the `vX.Y.Z` commit. **The attestation's `sourceRepositoryDigest` is the
  authoritative source commit**, not the per-contract tag and not the archive.
- **It creates the release before it attests the file**, so a failed attestation leaves a
  release with no attestation. Verification refuses it; recovery is above.

Bumping the pin: resolve the new latest release
(`gh release view --repo stellar-expert/soroban-build-workflow`), its SHA
(`gh api repos/stellar-expert/soroban-build-workflow/commits/<tag> --jq .sha`), read
`.github/workflows/release.yml` at that SHA, then update the SHA in `release.yml` and
**append** it to `RELEASE_SIGNER_DIGESTS` in `scripts/lib/release.sh` (old releases keep
verifying against the old digest), and update the `--signer-digest` in `release.yml`'s
`release_description`. The CLI version in `ci.yml` (`STELLAR_CLI_VERSION`) should
follow the CLI that workflow uses.

`home_domain` is not set: MicroCrop serves no SEP-1 `stellar.toml` yet. Add
`home_domain: <bare domain>` to `release.yml` once one exists.

### How anyone checks a deployed contract

- **stellar.expert**: open the contract; its build info shows the source repository, the
  commit and the workflow run when the hash matches a registered build.
- **Stellar Lab, contract explorer**: the
  [build info](https://developers.stellar.org/docs/tools/lab/smart-contracts/contract-explorer#build-info)
  panel reads the same metadata and attestation.
- **By hand**:
  ```bash
  stellar contract fetch --id <C...> --network mainnet --out-file deployed.wasm
  stellar contract info meta --wasm deployed.wasm     # source_repo=github:Microcrop-Protocol/microcrop-soroban
  gh attestation verify deployed.wasm --repo Microcrop-Protocol/microcrop-soroban \
    --signer-workflow stellar-expert/soroban-build-workflow/.github/workflows/release.yml \
    --signer-digest e43b241f114bcf02151c30d61088506c6ad3ae0b --deny-self-hosted-runners
  # or without gh: https://api.github.com/repos/Microcrop-Protocol/microcrop-soroban/attestations/sha256:<sha256 of deployed.wasm>
  ```
  The verified output names the source commit (`sourceRepositoryDigest`) and tag
  (`sourceRepositoryRef`) to review.

---

## Operating the contracts (behaviour after the 2026-10 audit fixes)

**Amounts are 7-decimal USDC.** 1 USDC = `10_000_000` base units. `sum_insured` must be
between `1_000_000_000` (100 USDC) and `10_000_000_000_000` (1,000,000 USDC). The backend
and the Lit signer must both scale by `1e7` on Stellar; the Base path keeps `1e6`. A signer
still on `1e6` makes every determination fail `SumInsuredMismatch`.

**A verified claim becomes an obligation.** `submit_drought_determination` verifies every
check (MIGRATION_NOTES §9), marks the policy `Claimed` in PolicyManager and records one
obligation in the SettlementAdapter: `Owed`, for the payout amount, carrying the
determination digest. The PayoutReceiver checks the returned obligation field by field
(`ObligationNotRecorded`, 329). Watch the adapter's `obligation_recorded` event, or read it:

```bash
stellar contract invoke --id "$ADAPTER" --network testnet --send no \
  -- find_obligation --payout_receiver "$PR" --policy_id <id>     # -> obligation id, or null
stellar contract invoke --id "$ADAPTER" --network testnet --send no \
  -- get_obligation --obligation_id <obligation id>
```

**Funding and settlement are attestations.** The partner pays the farmer off-platform and
files a settlement report; the backend (Funder) then marks the obligation, `mark_funded` for a
partial settlement and `mark_settled` for a full one, each with a 32-byte reference hash
identifying that report (never a phone number or name). `Owed → Settled` directly is allowed. Repeating a mark with the same
reference is a no-op (webhooks arrive twice); a different reference is refused (504 / 505);
an all-zero reference is refused (506); anyone without `Role::Funder`, the obligation's own
org included, is refused (1).

```bash
stellar contract invoke --id "$ADAPTER" --network testnet --source backend \
  -- mark_funded  --caller <FUNDER_G...> --obligation_id <id> --reference_hash <64 hex>
stellar contract invoke --id "$ADAPTER" --network testnet --source backend \
  -- mark_settled --caller <FUNDER_G...> --obligation_id <id> --reference_hash <64 hex>
```

**Swapping the adapter.** `PayoutReceiver.set_settlement_adapter(caller, adapter)` (Admin)
accepts only an adapter already bound back to this PayoutReceiver
(`adapter.payout_receiver() == PR`), otherwise `SettlementAdapterNotBound` (328). So: deploy
the new adapter, `set_payout_receiver(PR)` on it, then switch. It works while paused (an
incident swap). Obligations already recorded stay in the old adapter and stay markable there
**only while the keeper keeps them alive**: after the swap the PayoutReceiver's keeper calls
reach the new adapter only (see *Storage rent: the keeper*, step 3).
A swap never reopens a paid policy: `PolicyPaid` lives in the PayoutReceiver, so a replay
after a swap is still `PolicyAlreadyPaid` (310).

**Rotating the adapter's PayoutReceiver.** `SettlementAdapter.set_payout_receiver` can be
called at any time. The old PayoutReceiver's next `record_obligation` fails 500 (fail closed);
its existing obligations keep their own `payout_receiver` and stay markable. Obligations are
keyed by (PayoutReceiver, policy id), so a new PayoutReceiver's policy ids never collide with
the old one's.

**Storage rent: the keeper.** Soroban charges rent up front for every TTL extension. The
contracts used to extend every entry, instance and code to the network maximum (~180 days)
on every touch, so the first ordinary call to each contract paid ~173 days of code rent
(362–833 XLM on testnet). They now follow a rent cap (`crates/shared/src/ttl.rs`,
MIGRATION_NOTES §10):

- ordinary calls extend instance + code only below 3 days, to 10 days (a safety net);
- admin and config calls extend nothing;
- data entries live as long as the policy needs them: `end_date + 30 days` per policy
  (clamped to the ~180-day network maximum), below 30 days to 60 days for open (Owed /
  Funded) obligations and their lookup and index entries, 60 days for role grants, the
  longest need of any policy (at least 60 days) for per-farmer and per-org entries. A Settled
  obligation is left to archive: it stays restorable and never reads as absent.

So **the backend must run a keeper**, as a repeatable job:

1. **Daily:** `extend_contract_ttl()` on PayoutReceiver. It extends PayoutReceiver and the
   SettlementAdapter below 23 days to 30 days, then calls PolicyManager's, which does the
   same for PolicyManager and PolicyNft. Above 23 days it is a no-op, so running it daily costs one
   transaction fee most days and about 7 days of code rent once a week (estimated testnet
   ≈ 119 XLM a week for all four, mainnet ≈ 14.7 XLM; MIGRATION_NOTES §10).
2. **For each PENDING or ACTIVE policy, and each policy with an Owed or Funded obligation,
   whose shortest TTL is under 30 days:** `extend_policy_ttl(policy_id)` on PayoutReceiver.
   It extends the policy's entries in PayoutReceiver, PolicyManager and PolicyNft to their
   need, never to the maximum, and, best effort, the policy's open obligation in the adapter
   (`find_obligation` + `extend_obligation_ttl`: the obligation, its lookup, its org index
   item and the org's count). An adapter that traps never breaks the keeper. The adapter's
   `extend_obligation_ttl(obligation_id)` is also permissionless and callable directly.
3. **After an adapter swap, for every PREVIOUS adapter that still has an obligation not
   Settled:** both calls above reach only the CURRENT adapter, so nothing else keeps the old
   one alive and its code, instance and open obligations would archive (every later mark then
   pays a restore). Until each of its obligations is Settled, the keeper must call
   `extend_contract_ttl()` on that old adapter daily, and `extend_obligation_ttl(id)`
   directly on the adapter recorded with the claim (`claim.adapterContractId` in the
   backend), not via `PayoutReceiver.extend_policy_ttl`. A keeper that selects policies by
   obligation TTL and only calls `extend_policy_ttl` would pick those policies on every pass
   and never extend them.

**The keeper must always submit.** Both calls only extend TTLs: simulation shows no ledger
write, no event and no auth, so a client that sends only "state-changing" simulations never
sends them. The stellar CLI (28.x) does exactly that by default and prints "Simulation
identified as read-only. Send by rerunning with `--send=yes`." Pass `--send=yes`; the
backend keeper must likewise sign and submit the transaction regardless of how the
simulation looks, and check `liveUntilLedgerSeq` afterwards (`verify-deployment.sh` step 6
fails when any instance or code entry has under 23 days left).

```bash
stellar contract invoke --id "$PR" --network testnet --source <any funded key> --send=yes \
  -- extend_contract_ttl
stellar contract invoke --id "$PR" --network testnet --source <any funded key> --send=yes \
  -- extend_policy_ttl --policy_id <id>
```

If the keeper lapses, nothing breaks, but it gets expensive: each contract a call touches
tops itself up separately (below 3 days to 10 days), so a call pays up to ~10 days of code
rent **for each contract it touches**. A determination touches all four: estimated
≈ 150–170 XLM on testnet, ≈ 19–21 XLM on mainnet, charged to the relayer. Alert on keeper
failure. CLI fallback (the same as the keeper, without the contracts):

```bash
stellar contract extend --id C... --network testnet --source <key> \
  --durability persistent --ledgers-to-extend 518400            # instance, ~30 days
stellar contract extend --wasm-hash <hash> --network testnet --source <key> \
  --durability persistent --ledgers-to-extend 518400            # code entry
```

An entry that archives anyway is not lost and never reads as absent: it is restored
(`stellar contract restore`, or automatically by a transaction whose footprint includes it),
at a fee. Mainnet deploy budget: uploads pay the 120-day minimum (≈ 252 XLM for the four code
entries at the current sizes) and the keeper has nothing to do until about day 97.

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
So: sell at least ~140 days of cover and activate on or before planting day.
`create_policy` refuses Drought cover under 117 days (110 + 7; `InvalidDuration`, 106),
because such a policy takes a premium and can never settle; 117 is that floor, not the
recommended sale. Cover bought
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

**Premiums are off-chain.** There is no `receive_premium`: no contract takes custody of a
premium. On Stellar the partner collects the premium off-platform and attests it at
activation; the proof is the partner's own reference, held by the backend.

**Signer rule: one determination per policy per window.** Two determinations signed for the
same policy are both valid for an hour, and whichever is submitted first settles it. The
contract does not arbitrate. The Lit action must not sign a second determination for a
policy while an earlier one is within `MAX_REPORT_AGE` (3600 s) or the policy is paid.

**Expiry is permissionless.** `expire_policy(policy_id)` needs no role once
`now > end_date`; a keeper can run it to release the org's exposure and the farmer's slot.
This includes PENDING policies that were never activated (their `end_date` is creation time
plus duration); a lapsed PENDING policy can no longer be activated. Any premium refund is
off-chain.

**Every farmer needs their own address.** The open-policy cap (3, counting claims in the
last 365 days) is per farmer `Address`. If the backend falls back to one shared wallet for
farmers without a wallet, as the EVM backend does with `backendWallet`, the cap applies to
the whole platform.

**Cancelling ACTIVE cover is Admin-only.** The backend key can cancel PENDING policies. An
ACTIVE policy can be cancelled only by the admin key (escape hatch); a claimed policy cannot
be cancelled.

**Farmer cap.** A farmer may hold at most 3 open policies, counting claims paid in the last
365 days against the same cap. Expect `TooManyActivePolicies` for a farmer with recent
claims, not only for one with many open policies.

**Paged indexes.** `get_farmer_policies`, `get_distributor_policies` and the adapter's
`get_org_obligations` take `(start, limit)` and return at most 50 ids per call (a transaction
may touch at most 100 ledger entries); use the matching `*_count` view to page.

## Upgrading from a Treasury deployment: not supported in place

A Treasury-era deployment (the `deploy-v2` testnet deployment and the 2026-10-18 deploy) cannot be
upgraded to Option B in place: its PayoutReceiver was constructed with a Treasury, its
PolicyManager may hold `ClaimPending` (status 5, retired) policies, and the Treasury holds
USDC. **Deploy fresh** with `deploy.sh --state-dir <new dir>` and wind the old deployment
down: stop selling on it, let its policies expire (`expire_policy` is permissionless), settle
or refund any pending claim with its own (Treasury-era) tooling, and withdraw the remaining
reserves through the Treasury's admin functions. Those deployments stay Treasury-era; the
runbook for them is this file at `ed52203`.

`deploy.sh` and `verify-deployment.sh` refuse a state file that records a `treasury`.

**Upgrading an Option B deployment** (PolicyManager, SettlementAdapter, PayoutReceiver;
PolicyNft has no `upgrade`, as in the Solidity):

```bash
cargo build --target wasm32v1-none --release
for c in policy_manager settlement_adapter payout_receiver; do
  stellar contract optimize --wasm target/wasm32v1-none/release/microcrop_$c.wasm
done
HASH=$(stellar contract upload --wasm target/wasm32v1-none/release/microcrop_settlement_adapter.optimized.wasm --source admin --network testnet)
stellar contract invoke --id "$ADAPTER" --source admin --network testnet -- upgrade --caller <ADMIN_G...> --new_wasm_hash "$HASH"
# ...the same for PolicyManager and PayoutReceiver (Upgrader role), then:
stellar contract invoke --id "$PR" --source admin --network testnet --send=yes -- extend_contract_ttl
./scripts/verify-deployment.sh --network testnet
```

Each upload is a new code entry: it starts at the network minimum and the old code entry's
prepaid rent is lost, which is why the keeper target is 30 days and not the maximum.

**Rent checks after a deploy.** Simulate every entrypoint: nothing above ≈ 1.5 XLM except
the keeper; `extend_contract_ttl`'s cost should be about the days extended × 17 XLM on
testnet; `liveUntilLedgerSeq` of the instances and code entries ≈ now + 30 days
(`verify-deployment.sh` step 6).

**Signer.** Until the drought Lit action signs the Soroban preimage (`chain = stellar`,
`networkDomain`, the contract StrKey as `verifyingContract`) and its new CID is registered
with `scripts/rebuild-group.ts` (master key), no determination can verify. Check the live
PKP's public key equals `AUTHORIZED_SIGNER_PUBKEY` and its digest equals
`drought_determination_digest` for the deployed contract and domain.

## Testnet evidence (`scripts/testnet-evidence.sh`)

A positive drought determination on the real PolicyManager needs at least 110 days of ledger
time after `activate_policy` (plus ~7 days of archive lag), so it cannot be shown within days.
The evidence script runs two lanes, testnet only (it refuses `--network mainnet`, and the
fixture contract refuses a mainnet network id), with its own state file:

- **Lane P** (the deployment from `deploy.sh`): a real `create_policy` (Drought, 150 days) and
  `activate_policy`, then a determination for a season starting today. It must stop at
  **#327 SeasonNotComplete**, which fires before signature recovery: proof that validation
  runs on the live stack without the PKP.
- **Lane E** (evidence lane): the same SettlementAdapter and PayoutReceiver WASM (hashes
  compared with Lane P's) wired to `TestnetPolicyFixture`, a PolicyManager stand-in whose
  admin injects a policy (not a sale). A real 2022 ERA5 long-rains season, signed by a
  throwaway key only this lane trusts, runs end to end: `ObligationRecorded` (Owed),
  `mark_funded` and `mark_settled` by the Funder (after the org's own `mark_settled` is
  refused with #1); then the negative cases (#1 stranger, a direct `record_obligation` that no
  key can authorize (the PayoutReceiver is a contract address; the host-side refusal is
  proven by the unit test `record_needs_the_receivers_own_authorization`), #310 replay, #506 zero reference,
  #328 unbound adapter, a bound adapter swap followed by #310).

```bash
./scripts/deploy.sh --network testnet --state-dir <dir>/deploy-v3
./scripts/verify-deployment.sh --network testnet --state-dir <dir>/deploy-v3
./scripts/testnet-evidence.sh --lane-p-state <dir>/deploy-v3/testnet.json --state-dir <dir>/evidence
```

Never point it at the production PKP or the Lit account. Testnet is reset roughly quarterly.

**Live negative checks** on testnet: a season for a policy activated now → 327 (or 326 for a
season before activation); Comprehensive at sale → 118; a crop determination → 322.

## After a successful testnet deploy

1. Record the four addresses (and Stellar Expert links) — these are the Instaward Deliverable 1 evidence; the SettlementAdapter is Deliverable 2.
2. Give `NETWORK_DOMAIN` to whoever updates the Lit PKP signer. Nothing verifies until both sides match.
3. Run the cross-contract flow against live testnet and capture the transaction hash — Deliverable 3.
