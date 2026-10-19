//! Host-side helper for `scripts/testnet-evidence.sh`. TESTNET EVIDENCE ONLY.
//!
//! Builds the CROP_DROUGHT determination the evidence lanes submit, computes its digest with
//! the SAME encoder the contracts use (`microcrop_shared::encoding`), and signs it with a
//! THROWAWAY secp256k1 key. Never pass it the production PKP key: the evidence lane uses its
//! own signer (`set_authorized_signer` on the evidence-lane PayoutReceiver only).
//!
//! ```text
//! cargo run -q -p microcrop-integration-tests --example evidence_signer -- pubkey <sk-hex>
//! cargo run -q -p microcrop-integration-tests --example evidence_signer -- sign \
//!     <sk-hex> <payout-receiver C...> <domain-hex> <policy-id> <season-start> <assessed-at> \
//!     <lat-e6> <lon-e6> <sum-insured>
//! ```
//!
//! `sign` prints one JSON object: `digest` (hex), `signature` (64-byte r||s hex),
//! `recovery_id`, and `d_cli`, the determination in the stellar CLI's JSON argument format.
//! The evidence is the real 2022 ERA5 long-rains season at 1.0167 N / 35.0062 E (see
//! `real_era5_season_records_an_obligation_end_to_end`): W1 31130 / 27260, W2 16280 / 22305,
//! dry spell 7 days, drought 42 => 4,200 bp.

use k256::ecdsa::SigningKey;
use soroban_sdk::{Address, BytesN, Env};

use microcrop_shared::{
    encoding::{drought_determination_digest, DROUGHT_PARAMS_HASH_RDI_1_0},
    types::DroughtDetermination,
};

const SOURCE_HASH_ERA5_2022: [u8; 32] = [
    0x20, 0x7c, 0xa2, 0x6b, 0x24, 0x54, 0xf0, 0xd0, 0x8e, 0x04, 0x96, 0x2f, 0x42, 0x00, 0x8b, 0x73,
    0xba, 0x0a, 0x51, 0x74, 0x4f, 0x26, 0x73, 0xe9, 0x9a, 0x0e, 0xe9, 0xf5, 0xf5, 0x6a, 0xf8, 0x42,
];
const BP: u32 = 4_200;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim_start_matches("0x");
    assert!(s.len().is_multiple_of(2), "odd-length hex");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("bad hex"))
        .collect()
}

fn key(sk_hex: &str) -> SigningKey {
    SigningKey::from_slice(&unhex(sk_hex)).expect("secret key must be 32 bytes of hex")
}

fn arg<T: std::str::FromStr>(args: &[String], i: usize, name: &str) -> T {
    args.get(i)
        .unwrap_or_else(|| panic!("missing <{name}>"))
        .parse()
        .unwrap_or_else(|_| panic!("bad <{name}>"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pubkey") => {
            let sk = key(&args[1]);
            let ep = sk.verifying_key().to_encoded_point(false);
            println!("{}", hex(ep.as_bytes()));
        }
        Some("sign") => sign(&args),
        _ => {
            eprintln!("usage: evidence_signer pubkey <sk> | sign <sk> <pr> <domain> <policy-id> <season-start> <assessed-at> <lat-e6> <lon-e6> <sum-insured>");
            std::process::exit(2);
        }
    }
}

fn sign(args: &[String]) {
    let sk = key(&args[1]);
    let contract: String = arg(args, 2, "payout-receiver");
    let domain = unhex(&args[3]);
    assert_eq!(domain.len(), 32, "domain must be 32 bytes");
    let policy_id: u64 = arg(args, 4, "policy-id");
    let season_start: u64 = arg(args, 5, "season-start");
    let assessed_at: u64 = arg(args, 6, "assessed-at");
    let lat: i128 = arg(args, 7, "lat-e6");
    let lon: i128 = arg(args, 8, "lon-e6");
    let sum_insured: i128 = arg(args, 9, "sum-insured");
    let payout = sum_insured * BP as i128 / 10_000;

    let env = Env::default();
    let d = DroughtDetermination {
        on_chain_policy_id: policy_id,
        damage_percent_bp: BP,
        drought_damage: BP / 100,
        payout_amount: payout,
        assessed_at,
        latitude_e6: lat,
        longitude_e6: lon,
        sum_insured,
        season_start_epoch: season_start,
        lgp_days: 180,
        rain_w1_e2: 31_130,
        rain_ref_w1_e2: 27_260,
        rain_w2_e2: 16_280,
        rain_ref_w2_e2: 22_305,
        cdd_w2_days: 7,
        source_hash: BytesN::from_array(&env, &SOURCE_HASH_ERA5_2022),
        methodology_params_hash: BytesN::from_array(&env, &DROUGHT_PARAMS_HASH_RDI_1_0),
    };
    let mut dom = [0u8; 32];
    dom.copy_from_slice(&domain);
    let digest = drought_determination_digest(
        &env,
        &d,
        &Address::from_str(&env, &contract),
        &BytesN::from_array(&env, &dom),
    )
    .to_array();
    let (sig, recid) = sk.sign_prehash_recoverable(&digest).expect("sign");

    println!(
        concat!(
            "{{\"digest\":\"{}\",\"signature\":\"{}\",\"recovery_id\":{},\"d_cli\":{{",
            "\"on_chain_policy_id\":{},\"damage_percent_bp\":{},\"drought_damage\":{},",
            "\"payout_amount\":\"{}\",\"assessed_at\":{},\"latitude_e6\":\"{}\",",
            "\"longitude_e6\":\"{}\",\"sum_insured\":\"{}\",\"season_start_epoch\":{},",
            "\"lgp_days\":180,\"rain_w1_e2\":\"31130\",\"rain_ref_w1_e2\":\"27260\",",
            "\"rain_w2_e2\":\"16280\",\"rain_ref_w2_e2\":\"22305\",\"cdd_w2_days\":7,",
            "\"source_hash\":\"{}\",\"methodology_params_hash\":\"{}\"}}}}"
        ),
        hex(&digest),
        hex(&sig.to_bytes()),
        recid.to_byte(),
        policy_id,
        BP,
        BP / 100,
        payout,
        assessed_at,
        lat,
        lon,
        sum_insured,
        season_start,
        hex(&SOURCE_HASH_ERA5_2022),
        hex(&DROUGHT_PARAMS_HASH_RDI_1_0),
    );
}
