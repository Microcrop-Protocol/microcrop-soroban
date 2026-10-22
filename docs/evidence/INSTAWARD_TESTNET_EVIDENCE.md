# MicroCrop on Stellar: Instaward testnet evidence

Testnet run of 2026-10-22, on contracts deployed from release `v0.1.0`.

This replaces the write-up of the 2026-10-19 run. That run used a local build on contracts that have since been retired. Its version of this file is in the git history at `0a7d187`.

## Summary

- The four contracts (PolicyNft, PolicyManager, SettlementAdapter, PayoutReceiver) were deployed by `scripts/deploy.sh --release v0.1.0`. Before deploying, the script checked the GitHub build attestation on each release asset. `scripts/verify-deployment.sh` then passed 31 of 31 checks. One of those checks confirms that the code running on-chain is the attested release, built from `d7051b1`.
- The live PayoutReceiver accepts determinations only from the Lit PKP. That signer was set at deploy time and is confirmed by verify-deployment.
- A real policy was sold and activated on the live PolicyManager. Its determination is refused with #327 (season not complete), as it should be: a drought season takes 110 days and the policy is a day old.
- The full payout path ran on a second set of contracts, using the same release code, with a test signer and the real 2022 long-rains season. The PayoutReceiver verified the signed determination and recorded an obligation of 420 USDC as Owed. The Funder then marked it Funded and then Settled.
- Every refusal case behaved as expected: the insurer marking its own payout settled, an account with no role, a zero reference, three kinds of replay, and an adapter bound to another PayoutReceiver.
- No contract holds money. All three SettlementAdapters have 0 USDC and 0 XLM, and the adapter code has no way to call a token contract.

## Release and network

| item | value |
|---|---|
| release | `v0.1.0`, built by the SEP-55 release workflow from commit [`d7051b1`](https://github.com/Microcrop-Protocol/microcrop-soroban/commit/d7051b195ef1ead3feadebec97b09fb9b1ec391c) |
| network | Stellar testnet, passphrase `Test SDF Network ; September 2015` |
| network_domain | `cee0302d59844d32bdca915c8203dd44b33fbb7edc19051ea37abedf28ecd472` (sha256 of the passphrase). Every determination is signed over it, so a testnet signature cannot be replayed on mainnet. |
| live signer | Lit PKP, SEC-1 public key `04a2f05aa5a0b831473d4885f175044d74ff99b637ab0344d8a8f815a94d8d54079e7ffea716040c74888cabe6ecbeb8532fea7f0b8c8fb038e840c7309b977d19` |
| test signer (second set only) | secp256k1 key generated for this run, public key `04ca9daf8d32fbe2dd0cdffcabba07028e9d7be960ea600ab7965c8495754536ee8fc7cd145e4767907b172dfaef7809b35fa28725156a974b6d67df3df216d796`. The secret was never committed. |

| account | role | address |
|---|---|---|
| alice | Admin and Upgrader on every contract | `GDNFTJ73BDOFY34AKX73BBJODKY5MM2CDU5ESMAKKBZWREDKFIVOSCC6` |
| backend | Backend on PolicyManager, Funder on the SettlementAdapters | `GDP3C2TQWKU7WQYQO6DELJ4C6JFGWMV3ODKNW7DZ3Z4GLCQH4RDMZNIT` |
| relayer | Relayer on the PayoutReceivers | `GAGARTIEXV4VNOIUOXIPPXYI6AGAY5UBYLUXCIBY5UJ22BJX5X2RYKAL` |
| evidence-org | the insurer that owes the obligation | `GCSZGTZDT6RYO45ARDUJSHUWWFA5VD33V72TXAAGCZXZ3TYGCA7TGTIF` |
| evidence-farmer | the insured farmer | `GA77BEOQ26IA6Z5CPQL7G2BSYI7Y3SRIONSJBEAARN7KOD3NRW7M7WDG` |
| evidence-stranger | no role | `GB2IKBYDOKIVO4FBUYBXO2GJUMF7JEHJBNY5P7HPQXJTKU2H7CX2QIW5` |

## Contracts

### The live deployment

| contract | address | wasm sha256 (equals the release asset) |
|---|---|---|
| PolicyNft | [CB6FEFIXBDID45GVVCDWMLNKFBIWIJEDVFPIUJDAMRIISYWEA5FQ3HV4](https://stellar.expert/explorer/testnet/contract/CB6FEFIXBDID45GVVCDWMLNKFBIWIJEDVFPIUJDAMRIISYWEA5FQ3HV4) | `cfe38b89800f261ad9c69d9070dc0f3cb8f8d9f5e2984126b6b00aa6b7ba863a` |
| PolicyManager | [CDGQWA6QC2BRR5HHFJVNWAY4C34WXH7BCUFGHRDB4KT4WDMVUMDGDBSR](https://stellar.expert/explorer/testnet/contract/CDGQWA6QC2BRR5HHFJVNWAY4C34WXH7BCUFGHRDB4KT4WDMVUMDGDBSR) | `4845cfc0c6fa826ff31ffda18fbde4ac966a9a22993d83c79f856269423187b0` |
| SettlementAdapter | [CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5](https://stellar.expert/explorer/testnet/contract/CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5) | `007a5a387bbee9fa45e2f57cd7fcba97621444715efdb2122700d3b9c5883a79` |
| PayoutReceiver | [CCBWJMQM42WA6AWYEBAJCP6JCJBDPB7TBWAAEEECIN4OOTZGRAELBVX3](https://stellar.expert/explorer/testnet/contract/CCBWJMQM42WA6AWYEBAJCP6JCJBDPB7TBWAAEEECIN4OOTZGRAELBVX3) | `4bd87149801feec7941967d10bc49969fb9cf9f865fa0a3fd831a42c34ebbe0c` |

Source for the adapter: [crates/settlement-adapter/src/lib.rs at d7051b1](https://github.com/Microcrop-Protocol/microcrop-soroban/blob/d7051b195ef1ead3feadebec97b09fb9b1ec391c/crates/settlement-adapter/src/lib.rs).

### The second set, used for the payout path

| contract | address | wasm sha256 |
|---|---|---|
| TestnetPolicyFixture | [CAOE45YNHTQJFOWV7GJ5ONDZJHGZX7NEE3JLDYTL47JCYXSCXTGJJJPQ](https://stellar.expert/explorer/testnet/contract/CAOE45YNHTQJFOWV7GJ5ONDZJHGZX7NEE3JLDYTL47JCYXSCXTGJJJPQ) | test-only build. It stands in for the PolicyManager, lets the admin write a policy directly, and refuses to deploy on mainnet. |
| SettlementAdapter-E | [CANZ3JHL2P5HKBZ44GFZE55ECTFH77XTLDEONLCWQMWBRLRLTDPHTMD2](https://stellar.expert/explorer/testnet/contract/CANZ3JHL2P5HKBZ44GFZE55ECTFH77XTLDEONLCWQMWBRLRLTDPHTMD2) | `007a5a38…3a79`, same as live |
| PayoutReceiver-E | [CDPXZQLGNJ25O6SUEYT65RE6BCHNQTSYNT6SLHTDOZJKTO24DJFWCWC4](https://stellar.expert/explorer/testnet/contract/CDPXZQLGNJ25O6SUEYT65RE6BCHNQTSYNT6SLHTDOZJKTO24DJFWCWC4) | `4bd87149…be0c`, same as live |
| SettlementAdapter-E2 (used for the adapter swap test) | [CBRINQ3QBXTHPPVEVKRQREUZOSTGKO6T6DLDP4TRXAUYLLJ3SJQY4AVE](https://stellar.expert/explorer/testnet/contract/CBRINQ3QBXTHPPVEVKRQREUZOSTGKO6T6DLDP4TRXAUYLLJ3SJQY4AVE) | `007a5a38…3a79`, same as live |

`scripts/testnet-evidence.sh` reads the wasm hash of each contract in this set from the chain and stops if it differs from the live one. It did not stop, so the payout path below ran on exactly the code that is deployed live.

## Deployment check

`scripts/verify-deployment.sh --release v0.1.0` result: `passed: 31   failed: 0`. It checks:

1. All four contracts are live.
2. The wiring runs in both directions: PolicyManager and PolicyNft point at each other, the PayoutReceiver points at the PolicyManager, and the PayoutReceiver and SettlementAdapter are bound to each other.
3. The roles the payout path needs are in place: Minter, Oracle, Funder, Backend, Relayer.
4. The authorized signer is the Lit PKP and the network_domain is correct.
5. The drought methodology is enabled and the retired crop-damage one is disabled.
6. Every contract instance and code entry has at least 23 days of TTL left (29 at the time of the check).
7. Each deployed wasm hash equals its attested release asset, and all four assets were built from the commit the tag points to.

## The flow

### Sale on the live PolicyManager

| step | result | tx |
|---|---|---|
| `create_policy` by backend: drought, 150 days, 1,000 USDC sum insured, plot at 1.0167 N 35.0062 E | policy 1 created. No premium moves on-chain. | [a735c342…](https://stellar.expert/explorer/testnet/tx/a735c342214893a2fbed937e91c3467fa00beb9f014af35fa21c45a71a46fef0) |
| `activate_policy` by backend | NFT minted, policy Active | [8db8e257…](https://stellar.expert/explorer/testnet/tx/8db8e2576a31672b529c5c83cc1dc60a7a7b83e27444017a52b9c027bef32e09) |
| `submit_drought_determination` by relayer | refused with #327 SeasonNotComplete. This check runs before signature recovery, so it fires even though the test signer is not the PKP. | simulated only, no tx |
| afterwards | the live adapter's `obligation_count` is 0 | view call |

### Determination, then Owed, Funded and Settled

The policy is written into the fixture as policy 1: drought, Active, owed by evidence-org, farmer evidence-farmer, same plot and sum insured as the live sale, cover starting 2022-04-01. The determination is the real 2022 long-rains season for that plot from ERA5 data. The test key signed it over PayoutReceiver-E's address and the network_domain.

1. Before submitting, the contract's own `drought_determination_digest` view was called. It returned `8713e294019282f8cbe80ebaf0f09ebcdecc82c68132b701caba77e9570b8280`, the same digest the off-chain encoder produced.
2. `submit_drought_determination` by relayer: [5d431d91…](https://stellar.expert/explorer/testnet/tx/5d431d918de7b13c3e2be0fb9d7447b6a5168955c5a49afce584d511af39734d), ledger 5,326,528. In one transaction it emitted:
   - `DroughtEvidenceRecorded`: policy 1, damage 42%, season start 1648771200, source hash `207ca26b…f842`
   - `ObligationRecorded` on SettlementAdapter-E: obligation 1, amount 4,200,000,000 (420 USDC at 7 decimals, 42% of the sum insured), org evidence-org, with the determination digest above
   - `DeterminationVerified`, `DamageReportReceived` and `ObligationRequested` on PayoutReceiver-E

   The policy moved to Claimed, and `get_obligation(1)` returned status Owed.
3. `mark_funded` by backend (Funder): [2a0e2b7f…](https://stellar.expert/explorer/testnet/tx/2a0e2b7f46068af3f30feab9f95c16c1e00edd17271c18f73ff19c7e95059be7), ledger 5,326,532. Event `ObligationFunded`, reference hash `e7e882f2…5a7d`.
4. `mark_settled` by backend (Funder): [899d7cd8…](https://stellar.expert/explorer/testnet/tx/899d7cd89fc7ef741d188c367007b86cfaba576089c34588ea56ccf1435d2541), ledger 5,326,534. Event `ObligationSettled`, reference hash `0c913e42…30fe`.

The obligation as stored at the end:

```
{id:1, status:2 (Settled), policy_id:1, amount:4200000000,
 org:GCSZGTZDT6RYO45ARDUJSHUWWFA5VD33V72TXAAGCZXZ3TYGCA7TGTIF,
 payout_receiver:CDPXZQLGNJ25O6SUEYT65RE6BCHNQTSYNT6SLHTDOZJKTO24DJFWCWC4,
 determination_digest:8713e294019282f8cbe80ebaf0f09ebcdecc82c68132b701caba77e9570b8280,
 recorded_at:1792656227,
 funded_by:GDP3C2…ZNIT, funded_at:1792656247,
 funding_reference:e7e882f2f10f995ed7629bccbb3058344ff09f81a7ce4c58eb79871756855a7d,
 settled_by:GDP3C2…ZNIT, settled_at:1792656257,
 settlement_reference:0c913e424b5cd61c55d1378396b3d989da7dc6c172403474fc3d9d0300cf30fe}
```

The reference hashes are sha256 of a label (`microcrop:evidence:funding:v1:2026-10-22` and the settlement equivalent). In production they would be hashes of the M-Pesa payout receipt. The payment itself happens off-chain. Funded and Settled are statements by the Funder, not proof that money moved.

## Refusals

These were run as simulations. The CLI does not submit a transaction whose simulation fails, so there is no tx hash. The error is the contract's own.

| attempt | by | result |
|---|---|---|
| insurer marks its own obligation settled (obligation was Funded at the time) | evidence-org | #1 Unauthorized. Only the Funder can mark. |
| account with no role marks it funded | evidence-stranger | #1 |
| Funder marks with an all-zero reference hash | backend | #506 InvalidReference |
| same determination submitted again (policy is now Claimed) | relayer | #308 |
| policy reset to Active ([a7ad7014…](https://stellar.expert/explorer/testnet/tx/a7ad701491b54bbc624997c1f7972775251b44af703537f3e3f70c2dba916687)), then the same determination again | relayer | #310. The PayoutReceiver remembers the policy was paid. |
| point PayoutReceiver-E at the live adapter, which is bound to the live PayoutReceiver | alice | #328 SettlementAdapterNotBound |
| bind Adapter-E2 to PayoutReceiver-E ([8a66316e…](https://stellar.expert/explorer/testnet/tx/8a66316e2fd092417e1b604a7ecd524aa81dcff4fda6a8fa7bdeb106a3d16b39)), swap to it ([4e1cae0b…](https://stellar.expert/explorer/testnet/tx/4e1cae0b543f8410b0874fda239b26311e35b4f597452275e06d3bd07ae02b20)), then replay | relayer | #310, and Adapter-E2 still has 0 obligations. Swapping the adapter does not reopen a paid policy. |
| record an obligation claiming to be PayoutReceiver-E | alice | cannot be signed: no key can authorize as a contract address. The host-side refusal is covered by the unit test `record_needs_the_receivers_own_authorization`. |

A caller other than the bound PayoutReceiver gets #500 from `record_obligation`. That case is covered by the unit test `a_caller_other_than_the_receiver_is_refused_even_with_every_auth_mocked` and was not repeated on-chain in this run.

## No custody

| adapter | USDC (Circle testnet SAC `CBIELTK6…DAMA`) | XLM (native SAC `CDLZFC3S…CYSC`) |
|---|---|---|
| live `CCAALDRQ…KQF5` | 0 | 0 |
| `CANZ3JHL…TMD2`, after recording and settling a 420 USDC obligation | 0 | 0 |
| `CBRINQ3Q…4AVE` | 0 | 0 |

The balances alone don't show the adapter can't move money, so it was checked in two more ways:

- The release wasm for the adapter imports 28 host functions and none from module `d`. That module holds `call` and `try_call`, the only way a contract can invoke another contract, a token included. The PayoutReceiver wasm does import `d._` and `d.0`, because it calls the PolicyManager and the adapter.
- The unit test `the_adapter_never_touches_a_token_or_calls_another_contract` fails if the adapter source ever uses `token::`, `TokenClient`, `invoke_contract` or `Client::new`.

## Fees

All 41 transactions succeeded. Fees are Horizon's `fee_charged`.

| part | transactions | XLM |
|---|---:|---:|
| deploy and configure the live contracts | 20 | 225.21 |
| sale on the live PolicyManager | 2 | 5.81 |
| second set: deploy, configure, payout path, swap | 19 | 10.05 |
| total | 41 | 241.08 |

Of the deploy cost, 171.7 XLM is the first TTL extension to 30 days, in [26643ca0…](https://stellar.expert/explorer/testnet/tx/26643ca0674c74667b848c068c0912b16dd1601133a5f0ed840be220f02cb2b2). The four wasm uploads cost 9.5 to 18.6 XLM each. The payout path itself is cheap: the determination with its obligation record was 1.49 XLM, and each mark about 0.058 XLM. These are testnet figures; mainnet rent has not been measured.

Every transaction in this run:

| step | tx | ledger | fee (stroops) |
|---|---|---:|---:|
| PolicyNft upload | [d61e5ccb…](https://stellar.expert/explorer/testnet/tx/d61e5ccbaec325c0de5223e7d5aa1b9eec0a2f2fee5aebd206184eab6f602e65) | 5318534 | 95,014,207 |
| PolicyNft create | [154e9e68…](https://stellar.expert/explorer/testnet/tx/154e9e6817309efc95715aeb41cdb0f72ee0f9e3b367304419b1a9ae1ba447b3) | 5318535 | 1,219,765 |
| PolicyManager upload | [c5fa7e42…](https://stellar.expert/explorer/testnet/tx/c5fa7e420fd258bf5e4737db438c1a6d5a4d044f21667123752f0b9e15a1eb7a) | 5318539 | 133,879,085 |
| PolicyManager create | [7ff8a814…](https://stellar.expert/explorer/testnet/tx/7ff8a8143b5695d1deddb4846c72556947e6e8a1d0219194334da9d817b9be07) | 5318540 | 2,229,388 |
| SettlementAdapter upload | [bf02098e…](https://stellar.expert/explorer/testnet/tx/bf02098ec469b90b8f0cf9744d389f1fb78bf941f1d9b24a66c1e4faa33dfa09) | 5318542 | 106,747,095 |
| SettlementAdapter create | [317f7605…](https://stellar.expert/explorer/testnet/tx/317f7605b8e837755dd3e980649b21f8dd3ecd550d24d75785a19cd838d6e3a1) | 5318545 | 2,229,346 |
| PayoutReceiver upload | [47060f1c…](https://stellar.expert/explorer/testnet/tx/47060f1cbda18d3d81d583770e8bae7835b9de20ecb6c2b183644a1d1cfc7ec6) | 5318547 | 186,128,379 |
| PayoutReceiver create | [88211ab2…](https://stellar.expert/explorer/testnet/tx/88211ab261d9408d9c6751c1143b266075009aa42a19f89473fc988d3612c63b) | 5318549 | 2,354,814 |
| PolicyManager.set_policy_nft | [760ce8f2…](https://stellar.expert/explorer/testnet/tx/760ce8f2ecbf5c2f0a5853d0355ff760c7d95dca1b16173ba7993aa2972cb727) | 5318552 | 66,600 |
| PolicyNft.set_policy_manager | [252dea98…](https://stellar.expert/explorer/testnet/tx/252dea98f92f02917a8f027059403b0fb8ccc6fb4b303639cec4ede542a68594) | 5318553 | 70,078 |
| SettlementAdapter.set_payout_receiver | [6a30fbce…](https://stellar.expert/explorer/testnet/tx/6a30fbce5e6752119e00359131375e587b4884d26d5882ec3ae23fb563e319ff) | 5318557 | 70,192 |
| grant Backend on PolicyManager | [8c9fcea8…](https://stellar.expert/explorer/testnet/tx/8c9fcea8d3f8af56cf195f8fafe56add9665b13a4b46341709e33eacbd0121c1) | 5318558 | 1,046,929 |
| grant Minter on PolicyNft to PolicyManager | [576b294f…](https://stellar.expert/explorer/testnet/tx/576b294f0d047f70f0871bb2ba3e7fd5224e0fa5813edfe7e303824cbc69cf7d) | 5318559 | 1,017,112 |
| grant Oracle on PolicyManager to PayoutReceiver | [91ba80ae…](https://stellar.expert/explorer/testnet/tx/91ba80ae0cf94f68fc6dde5b604cea9e3e1eaaee48e4d0853da79c62b3a737fc) | 5318563 | 1,017,142 |
| grant Relayer on PayoutReceiver | [acb43251…](https://stellar.expert/explorer/testnet/tx/acb432517409ed67f652ffe0838bff664fbb822274f9d82716979edd694f36d3) | 5318564 | 1,046,936 |
| grant Funder on SettlementAdapter | [6f0c1f56…](https://stellar.expert/explorer/testnet/tx/6f0c1f564b65013e2670fc614aff8a6d0d8c0bca06ddf195fd04f24ebd2086eb) | 5318566 | 1,047,386 |
| set signer to the Lit PKP | [b9ba0338…](https://stellar.expert/explorer/testnet/tx/b9ba033477229c82b5f5671ffa39c54d3504ceab358bb12ee45ef9cae7d4bb0d) | 5318569 | 101,748 |
| set network_domain | [061759b8…](https://stellar.expert/explorer/testnet/tx/061759b89e724a637cb04a7a597e7adc9a13ee991b98b770fc1e46e6a2ea6178) | 5318570 | 69,893 |
| enable drought methodology | [1f0236cc…](https://stellar.expert/explorer/testnet/tx/1f0236ccc53a7e8dd815deb253db208999080ac79bdd1ae4a0a3143d14e6de83) | 5318571 | 52,795 |
| extend TTL of all four to 30 days | [26643ca0…](https://stellar.expert/explorer/testnet/tx/26643ca0674c74667b848c068c0912b16dd1601133a5f0ed840be220f02cb2b2) | 5318575 | 1,716,732,604 |
| create_policy | [a735c342…](https://stellar.expert/explorer/testnet/tx/a735c342214893a2fbed937e91c3467fa00beb9f014af35fa21c45a71a46fef0) | 5326481 | 29,013,896 |
| activate_policy | [8db8e257…](https://stellar.expert/explorer/testnet/tx/8db8e2576a31672b529c5c83cc1dc60a7a7b83e27444017a52b9c027bef32e09) | 5326483 | 29,108,599 |
| fixture upload | [83ba8183…](https://stellar.expert/explorer/testnet/tx/83ba8183d361c97a1d266b1faeece07cc066ceccacc49e34cfbc8c03896b8f84) | 5326493 | 68,529,309 |
| fixture create | [e914ae50…](https://stellar.expert/explorer/testnet/tx/e914ae50a4ce35f5b5b3e43d5ac7545ef18f748fd5ad34a1b08d32aa5de0827f) | 5326494 | 1,159,970 |
| Adapter-E create (release wasm already on-chain) | [c314ec31…](https://stellar.expert/explorer/testnet/tx/c314ec317f54a542ef777e1741fd5138cd7ff53557f8cc184f7fedf0dd59d788) | 5326498 | 2,269,105 |
| PayoutReceiver-E create | [6bf13e68…](https://stellar.expert/explorer/testnet/tx/6bf13e68ec6cc47724001d8116c6a040baba2b8715c46e49ebe695bd18c710ef) | 5326503 | 2,396,813 |
| Adapter-E.set_payout_receiver | [8019fc48…](https://stellar.expert/explorer/testnet/tx/8019fc486d9a59855a5d2f6b5afc05cdb5bddb8008d4eb37f1f6c9911963cdd1) | 5326505 | 71,377 |
| grant Oracle on fixture to PayoutReceiver-E | [1aedfc79…](https://stellar.expert/explorer/testnet/tx/1aedfc7967e01f0777691dc5bc375d600a356286a535a05fa90a3274da1b75ba) | 5326509 | 1,035,235 |
| grant Relayer on PayoutReceiver-E | [7b05b94f…](https://stellar.expert/explorer/testnet/tx/7b05b94fdf5983a0267097072f8221d35496499c34da77da7bb0f7fc4c8a052f) | 5326510 | 1,065,602 |
| grant Funder on Adapter-E | [3a4f1fea…](https://stellar.expert/explorer/testnet/tx/3a4f1fea2a0e23c20a3eb203793d4e6e44b2d3002c5cd789ebb9694df0227cf6) | 5326511 | 1,066,052 |
| set signer to the test key | [453cb87a…](https://stellar.expert/explorer/testnet/tx/453cb87ab2a7c64e52eaa7ac59d14717ffd7af7ea18d9b122a34fd2a9ef58d90) | 5326515 | 103,497 |
| set network_domain | [e2fe7c57…](https://stellar.expert/explorer/testnet/tx/e2fe7c570662c205e1822778a1cf27b6e8df28b8bea4cb9d985bcb9301c368de) | 5326517 | 71,079 |
| enable drought methodology | [6148eda5…](https://stellar.expert/explorer/testnet/tx/6148eda5859b8bd7416a5ce7c24ed543ea533102b490a7d0f547c3d8137f353b) | 5326521 | 53,667 |
| write policy 1 into the fixture | [ed583bfc…](https://stellar.expert/explorer/testnet/tx/ed583bfce348945ee0be4c6fe3e1ec5c9fec7737b85a20a81538cbd4457b9b44) | 5326523 | 4,217,972 |
| submit_drought_determination (Owed) | [5d431d91…](https://stellar.expert/explorer/testnet/tx/5d431d918de7b13c3e2be0fb9d7447b6a5168955c5a49afce584d511af39734d) | 5326528 | 14,941,596 |
| mark_funded | [2a0e2b7f…](https://stellar.expert/explorer/testnet/tx/2a0e2b7f46068af3f30feab9f95c16c1e00edd17271c18f73ff19c7e95059be7) | 5326532 | 582,765 |
| mark_settled | [899d7cd8…](https://stellar.expert/explorer/testnet/tx/899d7cd89fc7ef741d188c367007b86cfaba576089c34588ea56ccf1435d2541) | 5326534 | 583,075 |
| reset policy 1 to Active (for the replay test) | [a7ad7014…](https://stellar.expert/explorer/testnet/tx/a7ad701491b54bbc624997c1f7972775251b44af703537f3e3f70c2dba916687) | 5326539 | 5,933 |
| Adapter-E2 create | [8321fec7…](https://stellar.expert/explorer/testnet/tx/8321fec7c7b9814e7b8cccb9479280b290ee9a769b52633eb072b14970b152ca) | 5326541 | 2,268,360 |
| Adapter-E2.set_payout_receiver | [8a66316e…](https://stellar.expert/explorer/testnet/tx/8a66316e2fd092417e1b604a7ecd524aa81dcff4fda6a8fa7bdeb106a3d16b39) | 5326545 | 71,356 |
| PayoutReceiver-E.set_settlement_adapter(Adapter-E2) | [4e1cae0b…](https://stellar.expert/explorer/testnet/tx/4e1cae0b543f8410b0874fda239b26311e35b4f597452275e06d3bd07ae02b20) | 5326549 | 4,852 |

## Per SOW deliverable

**Deliverable 1: contracts on Stellar testnet, a network_domain, and a Soroban-format signer.** The four contracts above are live, wired and checked, and they run the attested `v0.1.0` code. The network_domain is set. The live PayoutReceiver takes determinations only from the Lit PKP, given as a 65-byte SEC-1 public key; the contract recovers the signer from a 64-byte signature and a recovery id. The signature path was exercised on-chain in [5d431d91…](https://stellar.expert/explorer/testnet/tx/5d431d918de7b13c3e2be0fb9d7447b6a5168955c5a49afce584d511af39734d) with a test key, on the same PayoutReceiver code.

**Deliverable 2: the SettlementAdapter.** Live at [CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5](https://stellar.expert/explorer/testnet/contract/CCAALDRQWXFAIOS7NMYCGJL2QKKTIKSJSX5DXFDTMIJWHV5FMGKOKQF5), source in `crates/settlement-adapter`. It replaces the old Treasury, which held insurers' money. The adapter holds nothing. It is a public record of what each insurer owes after a verified drought, and of whether that debt has been paid. Only the PayoutReceiver can create a debt, and only after verifying a signed determination. Only the Funder can mark it funded or settled, and it must attach a reference to the off-chain payment. The insurer that owes the debt cannot mark it settled.

**Deliverable 3: end-to-end on testnet.** In order: [create_policy](https://stellar.expert/explorer/testnet/tx/a735c342214893a2fbed937e91c3467fa00beb9f014af35fa21c45a71a46fef0) and [activate_policy](https://stellar.expert/explorer/testnet/tx/8db8e2576a31672b529c5c83cc1dc60a7a7b83e27444017a52b9c027bef32e09) on the live PolicyManager; the [signed determination recording obligation 1 as Owed](https://stellar.expert/explorer/testnet/tx/5d431d918de7b13c3e2be0fb9d7447b6a5168955c5a49afce584d511af39734d); [mark_funded](https://stellar.expert/explorer/testnet/tx/2a0e2b7f46068af3f30feab9f95c16c1e00edd17271c18f73ff19c7e95059be7); [mark_settled](https://stellar.expert/explorer/testnet/tx/899d7cd89fc7ef741d188c367007b86cfaba576089c34588ea56ccf1435d2541). The integration tests for the same flow are in [crates/integration-tests](https://github.com/Microcrop-Protocol/microcrop-soroban/tree/master/crates/integration-tests).

## What this run does not show

1. **A payout on the live sale.** `activate_policy` starts cover at the moment of activation, and a drought determination needs a complete 110-day season inside the cover. The earliest the live policy can be assessed is about 2027-02-09, if its TTL is kept alive and testnet isn't reset first. That is why the payout path ran on the second set, where the fixture writes the policy in directly instead of selling it.
2. **A PKP-signed determination on-chain.** The live PayoutReceiver is set to the PKP, but nothing has been submitted to it yet, for the reason above. The second set used a test key so that the production signer was never attached to a test contract.
3. **The insurer marking its own payment.** The code doesn't allow it: only the Funder can mark. The run shows the insurer's attempt failing.
4. **A real payment behind Settled.** The reference hashes here are of labels, not of M-Pesa receipts. The chain records the Funder's statement; it cannot see the off-chain payment.
5. **The backend driving the flow.** The backend writer and keeper for these contracts are merged in microcrop-core ([#97](https://github.com/Microcrop-Protocol/microcrop-core/pull/97), [#98](https://github.com/Microcrop-Protocol/microcrop-core/pull/98)), but this run was driven from the CLI. Pointing the backend at these addresses is a configuration step still to do.

## Reproducing

```bash
ADMIN_KEY=alice BACKEND_ADDR=$(stellar keys address backend) RELAYER_ADDR=$(stellar keys address relayer) \
AUTHORIZED_SIGNER_PUBKEY=0x04a2f05a…7d19 \
  ./scripts/deploy.sh --network testnet --release v0.1.0 --state-dir deployments/v0.1.0

BACKEND_ADDR=… RELAYER_ADDR=… AUTHORIZED_SIGNER_PUBKEY=0x04a2f05a…7d19 \
  ./scripts/verify-deployment.sh --network testnet --release v0.1.0 --state-dir deployments/v0.1.0

ADMIN_KEY=alice BACKEND_KEY=backend RELAYER_KEY=relayer \
  ./scripts/testnet-evidence.sh --network testnet \
  --lane-p-state deployments/v0.1.0/testnet.json --state-dir deployments/v0.1.0/evidence
```

The evidence script deploys the second set from `target/wasm32v1-none/release/*.optimized.wasm`. For this run those files were the `v0.1.0` release assets, copied in. The script's hash check fails if they are anything else.

Run logs and event dumps are kept by the team and available on request. Every transaction above can be checked on stellar.expert.
