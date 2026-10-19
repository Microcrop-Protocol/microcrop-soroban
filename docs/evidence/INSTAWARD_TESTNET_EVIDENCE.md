# MicroCrop on Stellar: Instaward evidence for the SettlementAdapter (Option B)

Testnet run, 2026-10-19 (ledgers 5,277,185 to 5,277,356). Code: microcrop-soroban branch `feat/settlement-adapter` at `c7a4065` ([PR #8](https://github.com/Microcrop-Protocol/microcrop-soroban/pull/8), open, not merged). No contract code was changed for this run.

## Summary

- **Deployed:** a fresh four-contract Option B stack (PolicyNft, PolicyManager, SettlementAdapter, PayoutReceiver) with the branch's own `deploy.sh`. `verify-deployment.sh` passed **30 of 30** checks, including the signer and the check that each deployed wasm hash equals the local build.
- **End to end, verified on-chain:** a signed drought determination was checked by the PayoutReceiver (secp256k1 recovery, network domain, digest). It recorded obligation #1 in the SettlementAdapter as **Owed**. The Funder then marked it **Funded** and then **Settled**, each with a reference hash. Every step has a tx hash, a Horizon `fee_charged` and an event.
- **No custody:** all three SettlementAdapters hold **0 USDC and 0 XLM**. The adapter wasm imports **no** contract-call host function at all, so it cannot move a token.
- **Negative cases:** a caller that is not the PayoutReceiver gets **#500**. A caller without the Funder role gets **#1** when it tries to mark an obligation settled; that includes the org that owes the obligation. Replays get **#308/#310**, also after an adapter swap. A zero reference gets **#506**, and an adapter bound to another PayoutReceiver gets **#328**.
- **What could not be shown:** a positive determination against the **real** PolicyManager's own sale. It is blocked by ledger time (at least 110 days). That sale was made on-chain and its determination correctly stops at **#327 SeasonNotComplete**. The positive run therefore used an evidence lane: the same adapter and PayoutReceiver code (identical wasm hashes), wired to a testnet-only fixture that injects the policy. Details are in "What was not shown" below.
- **Org-signed marks, on purpose not shown:** the task asked for `mark_funded` / `mark_settled` by the org. Review fix `70af932` on this branch made both marks **Funder-only**, so the org is refused with #1. The run proves that refusal and has the Funder (the backend account) make both marks.

## Network

| item | value |
|---|---|
| network | Stellar testnet, passphrase `Test SDF Network ; September 2015` |
| network_domain (= sha256(passphrase), set on both PayoutReceivers) | `cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472` |
| authorized signer (both PayoutReceivers) | throwaway secp256k1 key generated for this run, SEC-1 `04487c79e2ccfb147cc5b2c9ec240959824da0d54e89cd49f52acabcec03f2f6a53361a214cbc074d4b2efcac1ca5db1caf21da047a6140cf9d0ee987306e947c2`. **Not** the Lit PKP. The secret was never committed and is held only locally. |
| admin (Admin + Upgrader) | alice `GDNFTJ73BDOFY34AKX73BBJODKY5MM2CDU5ESMAKKBZWREDKFIVOSCC6` |
| backend (Backend on PM, Funder on adapters) | `GDP3C2TQWKU7WQYQO6DELJ4C6JFGWMV3ODKNW7DZ3Z4GLCQH4RDMZNIT` |
| relayer (Relayer on PRs) | `GAGARTIEXV4VNOIUOXIPPXYI6AGAY5UBYLUXCIBY5UJ22BJX5X2RYKAL` |
| org | `GD5LFJHPQEDLC6MYMEMSJ24BQHF3VQOPVTQKSL4LNM3PQHZ56JU6RXRZ` |
| farmer | `GAJWHHOVOWFHQ5WZBOYX3IXJUEWOW7BI3E4FLDM7OTCYPHPHXLXYA2WC` |
| stranger (no role) | `GB2IKBYDOKIVO4FBUYBXO2GJUMF7JEHJBNY5P7HPQXJTKU2H7CX2QIW5` |

## Contracts

### Lane P: the production stack, deployed by `scripts/deploy.sh`

| contract | address | wasm hash (sha256 of the local `.optimized.wasm`, checked on-chain) |
|---|---|---|
| PolicyNft | [CC47Y7HULIONSTJQP22GODU7FPPFN34RQQBIYKB4IYWCCDBFJ3AMCJYM](https://stellar.expert/explorer/testnet/contract/CC47Y7HULIONSTJQP22GODU7FPPFN34RQQBIYKB4IYWCCDBFJ3AMCJYM) | `1cd911a4be28ec43ec4a46d9b04948f4c70d01d346b745e5f051650c7b61dc8a` |
| PolicyManager | [CBS4N27ZWRAFPQAXW6PLUT4I3HRREZSAPUK7UCPZJG7UFGJO5Q556UCM](https://stellar.expert/explorer/testnet/contract/CBS4N27ZWRAFPQAXW6PLUT4I3HRREZSAPUK7UCPZJG7UFGJO5Q556UCM) | `e714a4224639d9dfab2ee6887ca04c3b6d62352f73563a72cc3f3affd01761e9` |
| **SettlementAdapter** | [CAAVFAC6ML3ZR5DZHQUKKG5WEJJ7PANQ2XHZJ3PXP3AMBZVWUCEQVCV3](https://stellar.expert/explorer/testnet/contract/CAAVFAC6ML3ZR5DZHQUKKG5WEJJ7PANQ2XHZJ3PXP3AMBZVWUCEQVCV3) | `5e7d155e5ef7d9fe3c5fa822cc84ec939e937dac757cf594c0df0836bd738fc8` |
| PayoutReceiver | [CBA54XPGIFXOWGVO27LZ6VMGTVRCKVP7I7JJMA5AMRG2WSPXOGSSSIBL](https://stellar.expert/explorer/testnet/contract/CBA54XPGIFXOWGVO27LZ6VMGTVRCKVP7I7JJMA5AMRG2WSPXOGSSSIBL) | `3339369994e0db08498b73c1f542c43adc38143a6025a66d7ef38fc4ea9cb947` |

### Lane E: the evidence lane (same SettlementAdapter and PayoutReceiver wasm)

| contract | address | wasm hash |
|---|---|---|
| TestnetPolicyFixture (injects policy state; not a sale; refuses mainnet in its constructor) | [CBKNNOLB6AFZH5MHIM5WK2U5TMZ3ZIXQVE6S36ABNAYAIM5CPKFITWXC](https://stellar.expert/explorer/testnet/contract/CBKNNOLB6AFZH5MHIM5WK2U5TMZ3ZIXQVE6S36ABNAYAIM5CPKFITWXC) | fixture build |
| **SettlementAdapter-E** (holds obligation #1) | [CDBDMGQ62OSIQBPQKCDGS3DGG5Z5EBWN6O2UPWSL25GUJ4D4CX7KXRHQ](https://stellar.expert/explorer/testnet/contract/CDBDMGQ62OSIQBPQKCDGS3DGG5Z5EBWN6O2UPWSL25GUJ4D4CX7KXRHQ) | `5e7d155e…38fc8` (= Lane P) |
| PayoutReceiver-E | [CA6DG3W2QTJGIDQVJWK6U7LWGURUNOGUTTUM6E3FHOHXZ4MOTF5DJD2H](https://stellar.expert/explorer/testnet/contract/CA6DG3W2QTJGIDQVJWK6U7LWGURUNOGUTTUM6E3FHOHXZ4MOTF5DJD2H) | `33393699…cb947` (= Lane P) |
| SettlementAdapter-E2 (target of the adapter swap test) | [CA4EUGBZNP5PZD3HKN25YSR2J6FFVIMNSCGZK32DINKHF2N4TYCROZWR](https://stellar.expert/explorer/testnet/contract/CA4EUGBZNP5PZD3HKN25YSR2J6FFVIMNSCGZK32DINKHF2N4TYCROZWR) | `5e7d155e…38fc8` |

The evidence script compared each Lane E instance's on-chain wasm hash with Lane P's and stopped if they differed. They match, so the PayoutReceiver and SettlementAdapter code that produced the positive result is byte for byte the code deployed by `deploy.sh`.

**SettlementAdapter source:** [crates/settlement-adapter/src/lib.rs @ c7a4065](https://github.com/Microcrop-Protocol/microcrop-soroban/blob/c7a4065d826eeaf0821ef48dfcd3383b9ff668fe/crates/settlement-adapter/src/lib.rs) (public repo; branch [feat/settlement-adapter](https://github.com/Microcrop-Protocol/microcrop-soroban/tree/feat/settlement-adapter); PR #8 open, not merged).

## verify-deployment.sh (Lane P)

`passed: 30   failed: 0 — Deployment verified.` It checked:
- the four contracts are live;
- PR→PM, PM↔NFT, `PR.settlement_adapter() == Adapter` and `Adapter.payout_receiver() == PR`;
- Minter, Oracle, Funder, Backend and Relayer roles;
- the signer matches the throwaway pubkey, and the network domain matches;
- drought enabled, crop disabled;
- every instance and code entry has more than 23 days of TTL (29 d);
- each deployed wasm hash equals the local optimized build.

Full log: `deploy-v3/verify.log`.

## End-to-end flow

### Lane P: real sale on the real PolicyManager

| step | result | tx |
|---|---|---|
| `create_policy` (backend; Drought, 150 d, SI 1,000 USDC, plot 1.0167 N / 35.0062 E, org = org) | policy id 1. Event `PolicyCreated`. No premium on-chain. | [37f622bb…](https://stellar.expert/explorer/testnet/tx/37f622bbe7cb679fab551a96310ec3444a66a02d3ccc98986cb30faaa41e7f72) |
| `activate_policy` (backend) | Events `PolicyNftMinted` (token 1) and `PolicyActivated`. Policy status 1 (Active). | [e3fa3cff…](https://stellar.expert/explorer/testnet/tx/e3fa3cff0395f2b01eedcd8d80afd937ca1a4a6ac1d1d03f63170f63c3c63e51) |
| `submit_drought_determination` (relayer, season start = today) | refused **#327 SeasonNotComplete** (simulation; nothing submitted). #327 fires before signature recovery, so validation steps 1-11 ran on the live stack. | log `evidence/logs/p_submit_season_not_complete.log` |
| afterwards | `Adapter.obligation_count() = 0`, `PR.is_policy_paid(1) = false`, policy still Active | view calls |

### Lane E: signed determination → obligation Owed → Funded → Settled

The fixture policy is id 1: Drought, Active, org = org, farmer = farmer, plot 1.016700 / 35.006200, SI 1,000 USDC, starting 2022-04-01. The determination is the real 2022 ERA5 long-rains season (W1 31130/27260, W2 16280/22305, CDD 7, 4200 bp, season start 1648771200). The throwaway key signed it over PayoutReceiver-E's address and the network domain.

1. **Digest agreement.** `PR-E.drought_determination_digest(d)` (a view) equalled the local encoder's digest, `472c1677390f5d83c71b023ab7485b863b300d158e13c1283dadaf416ebd44ae`, before submission.
2. **`submit_drought_determination`** (relayer): [f9b26492…](https://stellar.expert/explorer/testnet/tx/f9b264928429b780e68a4bedd24adb796925f0b5b25496e683fd93981f32e06b), ledger 5,277,317, fee_charged 13,833,269 stroops. One atomic transaction emitted:
   - PR-E `DroughtEvidenceRecorded{policy_id:1, drought_damage:42, season_start_epoch:1648771200, source_hash:207ca26b…f842}`
   - **Adapter-E `ObligationRecorded{obligation_id:1, policy_id:1, org:GD5L…RXRZ, payout_receiver:CA6D…JD2H, amount:4200000000, determination_digest:472c1677…44ae}`**, i.e. `record_obligation` called by the PayoutReceiver contract
   - PR-E `DeterminationVerified{policy_id:1, preimage_hash:472c1677…44ae}`
   - PR-E `DamageReportReceived{policy_id:1, farmer, damage_percentage:4200, payout_amount:4200000000}`
   - PR-E `ObligationRequested{policy_id:1, obligation_id:1, adapter:CDBD…XRHQ, amount:4200000000}`
   - The fixture policy moved to Claimed (4).

   `get_obligation(1)` afterwards: `status:0 (Owed)`, amount 4,200,000,000 (420 USDC at 7 dp, 42% of SI), and the digest equals the PR's preimage hash.
3. **`mark_funded(1, sha256("microcrop:evidence:funding:v1:2026-10-19"))`** by backend (Funder): [207b90ef…](https://stellar.expert/explorer/testnet/tx/207b90efcdbe0d0567e9dec85f8fb4660e06abe5461afd5477b71aa10241e24f), ledger 5,277,334, fee 534,770. Event `ObligationFunded{by: backend, reference_hash: e95e6915…ad00}`.
4. **`mark_settled(1, sha256("microcrop:evidence:settlement:v1:2026-10-19"))`** by backend (Funder): [2975028c…](https://stellar.expert/explorer/testnet/tx/2975028c7078b8864ecbfb80e0a8c021898ff48667270aa0c964d369d3bbedcb), ledger 5,277,339, fee 535,078. Event `ObligationSettled{by: backend, reference_hash: 60225691…833e, amount: 4200000000}`.

   Final `get_obligation(1)`:
   ```
   {id:1, status:2 (Settled), amount:4200000000, org:GD5LFJHPQEDLC6MYMEMSJ24BQHF3VQOPVTQKSL4LNM3PQHZ56JU6RXRZ,
    payout_receiver:CA6DG3W2QTJGIDQVJWK6U7LWGURUNOGUTTUM6E3FHOHXZ4MOTF5DJD2H, policy_id:1, org_index:0,
    determination_digest:472c1677390f5d83c71b023ab7485b863b300d158e13c1283dadaf416ebd44ae, recorded_at:1792410172,
    funded_by:GDP3C2…ZNIT, funded_at:1792410257, funding_reference:e95e691595679e2d3574003467b2c4715c6c8922d14ba849039007a07a0bad00,
    settled_by:GDP3C2…ZNIT, settled_at:1792410282, settlement_reference:60225691d7df79012e0a90fd59a70014bd954c0ba1145c027e52294c6a57833e}
   ```
5. **Adapter swap.** `PR-E.set_settlement_adapter(Adapter-E2)`: [ad67a36e…](https://stellar.expert/explorer/testnet/tx/ad67a36e0b99365dfa3e31e3663d53c2741f4d9b7d6636577910f15d45f69208), event `SettlementAdapterUpdated{previous: CDBD…XRHQ, adapter: CA4E…OZWR}`. It succeeded only after Adapter-E2 was bound back to PR-E ([5d29aa7f…](https://stellar.expert/explorer/testnet/tx/5d29aa7f2a6b851ea0b559dab0cc1e8a107edafa1e31049a7a9b6d75ab50b1cf)).

Event dumps (RPC `getEvents`): `deploy-v3/events-e.json` and `deploy-v3/events-p.json`.

## The adapter holds no tokens

| adapter | USDC balance (testnet Circle USDC SAC `CBIELTK6YBZJU5UP2WWQEUCYKLPU6AUNZ2BQ4WWFEIE3USCIHMXQDAMA`) | XLM balance (native SAC `CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC`) |
|---|---|---|
| Lane P `CAAVFAC6…VCV3` | 0 | 0 |
| Lane E `CDBDMGQ6…XRHQ` (after recording and settling a 420 USDC obligation) | 0 | 0 |
| Lane E2 `CA4EUGBZ…OZWR` | 0 | 0 |

The adapter also **has no token calls** in its code:
- **Wasm imports.** The optimized adapter wasm imports 28 host functions and **none from module `d`**, the module that holds `call` / `try_call`, the only way to invoke another contract such as a token. The PayoutReceiver wasm imports `d._` and `d.0` because it calls the adapter and the PolicyManager. So the adapter cannot reach a token contract even in principle.
- **Interface.** `stellar contract info interface` lists only: constructor, roles, upgrade, `set_payout_receiver`, `payout_receiver`, `record_obligation`, `mark_funded`, `mark_settled`, `get_obligation`, `find_obligation`, `obligation_count`, `org_obligation_count`, `get_org_obligations`, `extend_obligation_ttl` and `extend_contract_ttl`. There is no transfer, deposit or withdraw.
- **Unit test.** `the_adapter_never_touches_a_token_or_calls_another_contract` fails if `lib.rs` ever contains `token::`, `TokenClient`, `invoke_contract` or `Client::new`.

The amount is a unit of account only. Payment happens off-chain (M-Pesa), and `Funded` / `Settled` are **attestations** by the Funder, not proof of payment (MIGRATION_NOTES §11).

## Negative cases

These run as simulations: the CLI will not submit a transaction whose simulation fails, so nothing is spent and there is no tx hash. Each refusal is the contract's own error, captured in the logs listed.

| case | who | result | log |
|---|---|---|---|
| **non-PayoutReceiver caller records an obligation**: `record_obligation(caller=alice, …)` on the Lane P adapter | alice (admin) | **#500 NotPayoutReceiver** | `extra/neg_record_CAAVFAC6…log` |
| same, with caller = relayer, Lane P adapter | relayer | **#500** | `extra/neg_record_relayer_CAAVFAC6…log` |
| same two cases on the Lane E adapter | alice / relayer | **#500** / **#500** | `extra/neg_record_*CDBDMGQ6…log` |
| `record_obligation(caller=PR-E)` signed by alice: claims to be the PayoutReceiver without its authorization | alice | refused before submission: "Missing signing key for account CA6DG3…". No key can authorize as a contract address. The host-side refusal is proven by unit test `record_needs_the_receivers_own_authorization`. | `evidence/logs/e_neg_record_without_pr_auth.log` |
| **org marks its own obligation settled** | org | **#1 Unauthorized** (marks are Funder-only since `70af932`) | `evidence/logs/e_neg_org_marks_own_settled.log` |
| **non-Funder marks settled** | stranger | **#1** | `extra/neg_stranger_settled.log` |
| **non-Funder marks settled** | farmer | **#1** | `extra/neg_farmer_settled.log` |
| non-Funder marks funded | stranger | **#1** | `evidence/logs/e_neg_stranger_mark.log` |
| zero reference hash | backend (Funder) | **#506 InvalidReference** | `evidence/logs/e_neg_zero_reference.log` |
| replay of the same determination (policy now Claimed) | relayer | **#308** | `evidence/logs/e_neg_replay_claimed.log` |
| replay after re-injecting the policy as Active ([f808d571…](https://stellar.expert/explorer/testnet/tx/f808d5713050ec4105bf5e65b7b9a592f2328436f5f71c98f02b3e62cf76d9eb)) | relayer | **#310** (PR's own `PolicyPaid` guard) | `evidence/logs/e_neg_replay_paid.log` |
| `set_settlement_adapter` to Lane P's adapter, which is bound to another PR | alice | **#328 SettlementAdapterNotBound** | `evidence/logs/e_neg_adapter_bound_elsewhere.log` |
| replay after swapping to Adapter-E2 | relayer | **#310**; Adapter-E2 `obligation_count = 0` | `evidence/logs/e_neg_replay_after_swap.log` |

Note on the stranger and farmer `mark_settled` cases: they ran against obligation 1 after it was already Settled. The adapter checks authorization before the state transition, so they returned #1, not #505. The org's refusal ran while the obligation was still Funded, so it is the clean case.

## Every transaction, with Horizon fee_charged

#### Lane P: deploy and configure (deploy.sh)

| step | source | tx (stellar.expert) | ledger | fee_charged (stroops) | XLM |
|---|---|---|---|---:|---:|
| PolicyNft: upload wasm | alice (admin) | [ad00be1985662108…](https://stellar.expert/explorer/testnet/tx/ad00be198566210828569570e587a0d67d18e62a3db19d730ee22b1f490124f2) | 5277185 | 152,799,987 | 15.2800 |
| PolicyNft: create contract | alice (admin) | [d440a9dca1401399…](https://stellar.expert/explorer/testnet/tx/d440a9dca140139924698906395b743dc403155c97e8f68d79d2b07282ce3e95) | 5277188 | 1,137,251 | 0.1137 |
| PolicyManager: upload wasm | alice (admin) | [6fd27b25bbfd7157…](https://stellar.expert/explorer/testnet/tx/6fd27b25bbfd71576bc971929cab7e4148e169f490ba68a33408cb29345aae96) | 5277190 | 209,636,447 | 20.9636 |
| PolicyManager: create contract | alice (admin) | [f4825210be4f033d…](https://stellar.expert/explorer/testnet/tx/f4825210be4f033de919b84749e4507356e3590266417177a4c93f6d54f25cd3) | 5277193 | 2,078,307 | 0.2078 |
| SettlementAdapter: upload wasm | alice (admin) | [968858dea921ef2d…](https://stellar.expert/explorer/testnet/tx/968858dea921ef2d54ea30f34863f8c25bb0427369b902b572b560ee03c44d6a) | 5277195 | 165,557,321 | 16.5557 |
| SettlementAdapter: create contract | alice (admin) | [a9052227e4998bc1…](https://stellar.expert/explorer/testnet/tx/a9052227e4998bc186eaec1fdc8d3d453d1267326bcf305d638231ea03caa5a5) | 5277199 | 2,078,263 | 0.2078 |
| PayoutReceiver: upload wasm | alice (admin) | [51c92dd309650634…](https://stellar.expert/explorer/testnet/tx/51c92dd3096506342fe195fa60367b1c0d10f249ae8c66cd7f9f0b07e7be990c) | 5277201 | 270,937,933 | 27.0938 |
| PayoutReceiver: create contract | alice (admin) | [ef4e9891086a7a52…](https://stellar.expert/explorer/testnet/tx/ef4e9891086a7a5226f0663cc4de89014bb9ad7973a643b317bacc5a8fe4df1d) | 5277205 | 2,195,223 | 0.2195 |
| PolicyManager.set_policy_nft | alice (admin) | [cdb43cc7638930bc…](https://stellar.expert/explorer/testnet/tx/cdb43cc7638930bcb48414e8994038ffa263eb84c2cb1625a7845c7cfda3942b) | 5277212 | 62,347 | 0.0062 |
| PolicyNft.set_policy_manager | alice (admin) | [fd3591629ff80326…](https://stellar.expert/explorer/testnet/tx/fd3591629ff803267f7d8d3cfac5338bc40cfee50315906afb1e872269d4d9ab) | 5277214 | 65,587 | 0.0066 |
| SettlementAdapter.set_payout_receiver(PR) | alice (admin) | [9061f8c3fb910749…](https://stellar.expert/explorer/testnet/tx/9061f8c3fb910749eddd517ee6ea0b2a54ab4ba801d1a8af929ba11eb483b634) | 5277216 | 65,704 | 0.0066 |
| PM grant Backend(1) to backend | alice (admin) | [7e0c9ff5fece3ca9…](https://stellar.expert/explorer/testnet/tx/7e0c9ff5fece3ca9ae1c56c5fa4fa488f151f099b2933ada93a5ff916faf3fdc) | 5277218 | 976,001 | 0.0976 |
| NFT grant Minter(6) to PM | alice (admin) | [ebc2742eaaf63e1b…](https://stellar.expert/explorer/testnet/tx/ebc2742eaaf63e1b141afed536d40029cc03e3e6cef6d3d6591a7a2d4f372eaf) | 5277222 | 948,212 | 0.0948 |
| PM grant Oracle(2) to PR | alice (admin) | [a053b07f101622f6…](https://stellar.expert/explorer/testnet/tx/a053b07f101622f62503eaaafed7478a4188eb1804b9d1686495ae5849f724cd) | 5277224 | 948,242 | 0.0948 |
| PR grant Relayer(4) to relayer | alice (admin) | [103ed9cc115b6b73…](https://stellar.expert/explorer/testnet/tx/103ed9cc115b6b734bc441ec313c9a6bd12f372be3211476671e38256529ad1a) | 5277227 | 976,008 | 0.0976 |
| Adapter grant Funder(7) to backend | alice (admin) | [0037fb644c2fdbb6…](https://stellar.expert/explorer/testnet/tx/0037fb644c2fdbb65eed058d390ecef3fd94c657a1229a182e8be3fa7e3975e4) | 5277229 | 976,459 | 0.0976 |
| PR.set_authorized_signer (throwaway key) | alice (admin) | [3daf9d5c83bef6fc…](https://stellar.expert/explorer/testnet/tx/3daf9d5c83bef6fce4a1a589827ca7ac9560497a1ddd42c5bf9ccca5059cce73) | 5277233 | 95,128 | 0.0095 |
| PR.set_network_domain | alice (admin) | [fc89b6a202cda0f9…](https://stellar.expert/explorer/testnet/tx/fc89b6a202cda0f93743aae95e218f4a91d8e8fdeae3d7a9ab977bb594ce3924) | 5277235 | 65,402 | 0.0065 |
| PR.set_methodology_enabled(drought) | alice (admin) | [e0036c71597e18e7…](https://stellar.expert/explorer/testnet/tx/e0036c71597e18e7cd8fdf6de7fddc446409386cf8c7d682ecbd05eaff037169) | 5277239 | 49,486 | 0.0049 |
| PR.extend_contract_ttl (keeper: 4 contracts to 30 d) | alice (admin) | [8307be7e39e11b0c…](https://stellar.expert/explorer/testnet/tx/8307be7e39e11b0cdb45e9f60254585c7c5343b6483b592d841e00da5aa63933) | 5277241 | 2,626,888,787 | 262.6889 |
| **subtotal** | | | | **3,438,538,095** | **343.8538** |

#### Lane P: real sale on the real PolicyManager

| step | source | tx (stellar.expert) | ledger | fee_charged (stroops) | XLM |
|---|---|---|---|---:|---:|
| PolicyManager.create_policy (Drought, 150 d) | backend | [37f622bbe7cb679f…](https://stellar.expert/explorer/testnet/tx/37f622bbe7cb679fab551a96310ec3444a66a02d3ccc98986cb30faaa41e7f72) | 5277275 | 26,573,581 | 2.6574 |
| PolicyManager.activate_policy (NFT minted) | backend | [e3fa3cff0395f2b0…](https://stellar.expert/explorer/testnet/tx/e3fa3cff0395f2b01eedcd8d80afd937ca1a4a6ac1d1d03f63170f63c3c63e51) | 5277280 | 26,660,654 | 2.6661 |
| **subtotal** | | | | **53,234,235** | **5.3234** |

#### Lane E: evidence lane

| step | source | tx (stellar.expert) | ledger | fee_charged (stroops) | XLM |
|---|---|---|---|---:|---:|
| TestnetPolicyFixture: upload wasm | alice (admin) | [84b1336683d51297…](https://stellar.expert/explorer/testnet/tx/84b1336683d51297ed5d06a020c94fa020780278f33669f5c696f0d28c765c83) | 5277283 | 62,766,461 | 6.2766 |
| TestnetPolicyFixture: create contract | alice (admin) | [8c26dfb4e3b14c28…](https://stellar.expert/explorer/testnet/tx/8c26dfb4e3b14c2818cb8839d87f08b17f18cf5ffb2b3fef9c6d00708989facd) | 5277286 | 1,062,985 | 0.1063 |
| SettlementAdapter-E: create contract | alice (admin) | [8d90247ed59b9f74…](https://stellar.expert/explorer/testnet/tx/8d90247ed59b9f7479e2615c7e3858363efcbd3f26c358d8da25120415d3db86) | 5277290 | 2,079,009 | 0.2079 |
| PayoutReceiver-E: create contract | alice (admin) | [df41e5d69db85f79…](https://stellar.expert/explorer/testnet/tx/df41e5d69db85f7908cf0cacb46317466f3ba457748c5ac600c3abf8e697da1b) | 5277292 | 2,196,011 | 0.2196 |
| Adapter-E.set_payout_receiver(PR-E) | alice (admin) | [d35d17a57e3456b5…](https://stellar.expert/explorer/testnet/tx/d35d17a57e3456b51fba7da56b4b6adb3c03800e50b7a6546f8247d333e10ae3) | 5277297 | 65,732 | 0.0066 |
| Fixture grant Oracle(2) to PR-E | alice (admin) | [ff1116453fe9600a…](https://stellar.expert/explorer/testnet/tx/ff1116453fe9600a7e937899a5789a40283ecf90ec0fce81bb25d7c28785ea34) | 5277299 | 948,535 | 0.0949 |
| PR-E grant Relayer(4) | alice (admin) | [87aeb661dffb53e6…](https://stellar.expert/explorer/testnet/tx/87aeb661dffb53e686a9e934c9c7aff59cc91911516068f98f6f240941f554a9) | 5277302 | 976,358 | 0.0976 |
| Adapter-E grant Funder(7) to backend | alice (admin) | [79e7a0150923e37c…](https://stellar.expert/explorer/testnet/tx/79e7a0150923e37c36dd593024b7f93926166438848c4c6a180efc4db5a08e8b) | 5277304 | 976,809 | 0.0977 |
| PR-E.set_authorized_signer (throwaway key) | alice (admin) | [5aa98a18a4110568…](https://stellar.expert/explorer/testnet/tx/5aa98a18a4110568e9d2c9cb1785767af216df5be87c476f0c415dc12afa4e88) | 5277308 | 95,170 | 0.0095 |
| PR-E.set_network_domain | alice (admin) | [654c1aa49b9018ac…](https://stellar.expert/explorer/testnet/tx/654c1aa49b9018ac15bea867a482515bee225f38d2fa67ca9041830301cad17a) | 5277310 | 65,430 | 0.0065 |
| PR-E.set_methodology_enabled(drought) | alice (admin) | [eb7b3d8b8991e806…](https://stellar.expert/explorer/testnet/tx/eb7b3d8b8991e806f2c39c14f8ea33b3cf92c57fddece7e575c8f574ec0fc87a) | 5277313 | 49,529 | 0.0050 |
| Fixture.set_policy (inject policy 1, Active) | alice (admin) | [6e1f4874b2af0a2f…](https://stellar.expert/explorer/testnet/tx/6e1f4874b2af0a2f3b7be7e8c2ddca85a23d50ac02e53eb687ddfe7051270664) | 5277315 | 3,865,375 | 0.3865 |
| **PR-E.submit_drought_determination: signature verified, obligation 1 recorded (Owed)** | relayer | [f9b264928429b780…](https://stellar.expert/explorer/testnet/tx/f9b264928429b780e68a4bedd24adb796925f0b5b25496e683fd93981f32e06b) | 5277317 | 13,833,269 | 1.3833 |
| **Adapter-E.mark_funded(1, ref) by backend (Funder): Funded** | backend | [207b90efcdbe0d05…](https://stellar.expert/explorer/testnet/tx/207b90efcdbe0d0567e9dec85f8fb4660e06abe5461afd5477b71aa10241e24f) | 5277334 | 534,770 | 0.0535 |
| **Adapter-E.mark_settled(1, ref) by backend (Funder): Settled** | backend | [2975028c7078b886…](https://stellar.expert/explorer/testnet/tx/2975028c7078b8864ecbfb80e0a8c021898ff48667270aa0c964d369d3bbedcb) | 5277339 | 535,078 | 0.0535 |
| Fixture.set_policy (re-inject Active, for the 310 replay test) | alice (admin) | [f808d5713050ec41…](https://stellar.expert/explorer/testnet/tx/f808d5713050ec4105bf5e65b7b9a592f2328436f5f71c98f02b3e62cf76d9eb) | 5277345 | 5,933 | 0.0006 |
| SettlementAdapter-E2 (swap target): create contract | alice (admin) | [897e460b30d5c8e6…](https://stellar.expert/explorer/testnet/tx/897e460b30d5c8e61dd2d90b26dede90a80b0231e728d7583c6f9f79e5262971) | 5277350 | 2,080,003 | 0.2080 |
| Adapter-E2.set_payout_receiver(PR-E) | alice (admin) | [5d29aa7f2a6b851e…](https://stellar.expert/explorer/testnet/tx/5d29aa7f2a6b851ea0b559dab0cc1e8a107edafa1e31049a7a9b6d75ab50b1cf) | 5277352 | 65,764 | 0.0066 |
| PR-E.set_settlement_adapter(Adapter-E2) | alice (admin) | [ad67a36e0b99365d…](https://stellar.expert/explorer/testnet/tx/ad67a36e0b99365dfa3e31e3663d53c2741f4d9b7d6636577910f15d45f69208) | 5277356 | 4,862 | 0.0005 |
| **subtotal** | | | | **92,207,083** | **9.2207** |

All 41 transactions succeeded (Horizon `successful: true`). Total fee_charged: **3,583,979,413 stroops = 358.3979 XLM**.

**Notes on fees:**
- The four wasm uploads cost 15.3 to 27.1 XLM each.
- The single largest item is the keeper's first `extend_contract_ttl`: **262.7 XLM**, for 4 instances plus 4 code entries from about 7 days to 30 days of TTL. It matches the 370.5 XLM measured for the Treasury-era five-contract stack in PR #7.
- The claim path itself is cheap: determination plus obligation record 1.38 XLM, `mark_funded` 0.053 XLM, `mark_settled` 0.054 XLM.
- These are testnet resource fees. MIGRATION_NOTES §10's mainnet rent table is still not re-measured.

## Plain language, per SOW deliverable

**Deliverable 1: contracts on Stellar, a network domain, and a Soroban-format signer.**
- MicroCrop's four Stellar contracts are live on testnet and wired together. A checker confirmed every link, role and setting, and that the deployed code is exactly the code built from the branch.
- The **network domain** is a 32-byte fingerprint of the testnet network (`cee0302d…d472`). Every determination is signed over it, so a testnet signature can never be replayed on mainnet.
- The contract only accepts a determination signed with a secp256k1 key in the format the Soroban host verifies: the 65-byte SEC-1 public key, a 64-byte signature and a recovery id. The contract recovers the key itself.
- In this run, a throwaway key in that format signed a real 2022 drought season and the contract accepted it. The contract's own digest view matched the off-chain encoder byte for byte.
- **Separately proven, not repeated here:** the user's real Lit PKP signs a Stellar drought determination whose digest equals the live contract's `drought_determination_digest` byte for byte. This was recorded in the earlier oracle and PR #7 work (`crop-drought-rdi-1.0` with `chain:'stellar'`, 2026-10-19). This run deliberately did not use the PKP, so the production signer was never configured on or exposed to an evidence contract.

**Deliverable 2: the SettlementAdapter.**
- **Address:** Lane P [CAAVFAC6ML3ZR5DZHQUKKG5WEJJ7PANQ2XHZJ3PXP3AMBZVWUCEQVCV3](https://stellar.expert/explorer/testnet/contract/CAAVFAC6ML3ZR5DZHQUKKG5WEJJ7PANQ2XHZJ3PXP3AMBZVWUCEQVCV3). It is the same code as Lane E's [CDBDMGQ62OSIQBPQKCDGS3DGG5Z5EBWN6O2UPWSL25GUJ4D4CX7KXRHQ](https://stellar.expert/explorer/testnet/contract/CDBDMGQ62OSIQBPQKCDGS3DGG5Z5EBWN6O2UPWSL25GUJ4D4CX7KXRHQ), which holds the settled obligation.
- **Source:** [settlement-adapter/src/lib.rs @ c7a4065](https://github.com/Microcrop-Protocol/microcrop-soroban/blob/c7a4065d826eeaf0821ef48dfcd3383b9ff668fe/crates/settlement-adapter/src/lib.rs).
- **What it does:** it replaces the old custodial Treasury. It never holds money: its balances are zero and its code cannot call a token. It keeps a public, tamper-evident ledger of what each insurer owes each farmer after a verified drought, and tracks each debt from Owed to Funded to Settled.
- **Who can change it:** only the PayoutReceiver can create a debt, and only after verifying a signed determination. Only the paying agent (the Funder role) can mark it funded or settled, and it must attach a reference hash of the off-chain payment. Nobody else can, including the insurer that owes it.

**Deliverable 3: end-to-end transactions.** In order:
- [create_policy](https://stellar.expert/explorer/testnet/tx/37f622bbe7cb679fab551a96310ec3444a66a02d3ccc98986cb30faaa41e7f72) and [activate_policy](https://stellar.expert/explorer/testnet/tx/e3fa3cff0395f2b01eedcd8d80afd937ca1a4a6ac1d1d03f63170f63c3c63e51) on the real PolicyManager;
- the [signed determination, verified on-chain, recording obligation #1 as Owed](https://stellar.expert/explorer/testnet/tx/f9b264928429b780e68a4bedd24adb796925f0b5b25496e683fd93981f32e06b);
- [mark_funded](https://stellar.expert/explorer/testnet/tx/207b90efcdbe0d0567e9dec85f8fb4660e06abe5461afd5477b71aa10241e24f);
- [mark_settled](https://stellar.expert/explorer/testnet/tx/2975028c7078b8864ecbfb80e0a8c021898ff48667270aa0c964d369d3bbedcb) (status Settled).

The sale and the payout determination are on two different PolicyManagers, for the reason given below.

## What was not shown, and why

1. **A positive determination on the real PolicyManager's own sale.**
   - `activate_policy` sets the cover start to now. A drought determination needs a full 110-day season inside the cover, and must be assessed within an hour of submission.
   - The earliest positive result for policy 1 on Lane P is therefore about 2027-02-06, if the keeper keeps the TTLs alive and testnet is not reset first. Testnet resets roughly quarterly.
   - Today, the real sale's determination correctly stops at #327.
   - The positive path ran on Lane E, where a testnet-only fixture **injects** the policy (it is not a sale), against the identical PayoutReceiver and SettlementAdapter wasm.
2. **The org signing `mark_funded` / `mark_settled`.**
   - This is no longer allowed by the code (review fix `70af932`, Funder-only marks). Backend-onboarded org addresses are also keyless (sha256-derived), so they could never sign anyway.
   - The org's attempt is shown failing with #1, and the Funder made both marks.
3. **The Lit PKP signing for these contracts.** It was deliberately not used (throwaway key only). Its digest compatibility is the separately proven fact cited under Deliverable 1, not re-run here.
4. **A real M-Pesa receipt behind `mark_settled`.**
   - The reference hashes here are sha256 of evidence labels, not of a real payout.
   - "Settled" is an attestation by the Funder; the chain cannot prove the off-chain payment happened.
5. **Backend integration (spec §8).** Not started. No backend job drove these transactions; the CLI did.
6. **The host-side refusal of a forged PayoutReceiver authorization.**
   - On testnet the CLI cannot even build that transaction, because no key exists for a contract address.
   - The host's own refusal is proven only in the unit test `record_needs_the_receivers_own_authorization`.
7. **SEP-55 build attestation.** None exists (no CI). The wasm-hash equality in verify section 7 is a stand-in: it ties the deployment to a local build at `c7a4065`, not to a published release artifact.

## Bug found while running (script only, not contract code)

**`scripts/testnet-evidence.sh` exits silently on any read-only call.**
- **Cause:** line 130:
  ```bash
  LAST_HASH="$(printf '%s' "$LAST_OUT" | grep -o 'Signing transaction: [0-9a-f]*' | tail -1 | awk '{print $3}')"
  ```
  Under `set -euo pipefail`, a `--send no` view has no "Signing transaction" line. `grep` exits 1, the pipeline fails, the assignment fails, and the script exits with no message.
- **Effect:** the first run stopped right after the determination was submitted, at `find_obligation`. Lane P's view call survived only because it ends in `|| true`.
- **Workaround for this run:** a patched copy with `{ grep … || true; }`, resumed from the state file; it finished Lane E.
- **Fixed on this branch** in commit `aa77266`.
- **Side effect on the report:** the second (traced) run re-wrote the report header, so `evidence/evidence.md` has only the Lane E part from the resumed run. Run 1's Lane P table is in `evidence/evidence.run1.md`, and everything is restated above.

## Stellar skill checklist items this run demonstrates on-chain

From `stellar-dev/skills/smart-contracts/security.md` § Checklists → Contract:

| checklist item | shown by |
|---|---|
| All privileged functions require appropriate authorization, loaded from storage | #500 for every non-PR `record_obligation`; #1 for org, stranger and farmer marks; Funder role granted and checked from storage (verify §3) |
| Every layer using an address's authority re-auths it | `record_obligation` requires the PayoutReceiver's own auth; a forged caller cannot be signed (CLI) or authorized (unit test) |
| Initialization only once / `__constructor` | all contracts were deployed with constructor args in the same transaction (deploy txs above) |
| External calls allowlisted; return values sanity-checked | #328 on an adapter bound elsewhere; the swap is accepted only after the handshake; the obligation fields equal the determination (digest, amount, org, PR) |
| Arithmetic checked; input ranges validated | #506 on a zero reference; amount equals 4200 bp × SI exactly |
| Critical TTLs extended proactively | keeper tx `8307be7e…`; verify §6 shows 29 d on every instance and code entry |
| Events for auditable state changes | ObligationRecorded / Funded / Settled, SettlementAdapterUpdated, ObligationRequested, all captured from RPC |
| Deployed WASM traceable to its source | partially: wasm hash equals the local build at a pushed commit; no SEP-55 attestation yet (flag 1) |
| Emergency controls (pause) for value-bearing contracts | not applicable to the adapter (holds nothing, as shown above); the PR pause is unchanged |

testing.md: this is the "testnet rehearsal" and "real WASM" layer. The contracts under test are the optimized release wasm on a real network, not native rlib registrations.

## Raw logs

The raw run logs, event dumps (`getEvents`) and the per-transaction table with ledgers and fees are kept by the team and available on request. Every transaction above can be checked independently on stellar.expert.
