//! FROZEN canonical byte layouts for the PKP determination signatures (CROP_DAMAGE and
//! CROP_DROUGHT). The drought layout is documented at [`encode_drought_preimage`].
//!
//! Solidity builds the signed digest with `abi.encode` (inputsHash) and `abi.encodePacked`
//! (settlement preimage). Soroban has no `abi.*`, so this module defines an explicit,
//! frozen byte layout that both the on-chain verifier (`PayoutReceiver::submit_determination`)
//! and the off-chain Lit PKP signer MUST produce identically. Changing anything here breaks
//! every signature — treat as a wire format.
//!
//! ## Layout (mirrors `PayoutReceiver.sol` §10)
//!
//! Every scalar is a 32-byte **big-endian, two's-complement** word (matching an EVM 256-bit
//! slot). Signed fields sign-extend into the high 16 bytes; unsigned fields zero-pad.
//!
//! `inputsHash = keccak256( encode_inputs )`, where `encode_inputs` is the 10 evidence
//! fields in this exact order (identical to the Solidity `abi.encode`):
//!   on_chain_policy_id, latitude_e6, longitude_e6, sum_insured, ndvi_scaled,
//!   weather_present, weather_temp_c_e2, weather_precip_e2, weather_humidity, weather_wind_e2
//!
//! `preimageHash = keccak256( encode_preimage )`, where `encode_preimage` is:
//!   keccak256("1.0") ‖ keccak256("CROP_DAMAGE") ‖ keccak256("crop-dualindex-1.0")
//!   ‖ network_domain (32B)        // replaces EVM `block.chainid`
//!   ‖ contract_address (XDR bytes) // replaces `address(this)`
//!   ‖ inputsHash (32B)
//!   ‖ on_chain_policy_id ‖ damage_percent_bp ‖ weather_damage ‖ satellite_damage
//!   ‖ payout_amount ‖ assessed_at            // 6 result words
//!
//! ## Money units
//! `sum_insured` and `payout_amount` are USDC **base units at 7 decimals** (Stellar Asset
//! Contract; 1 USDC = 10_000_000). The EVM path signs 6-decimal amounts. A Lit signer that
//! scales by 1e6 for Stellar produces values 10x too small: `sum_insured` then fails step 6b
//! (`SumInsuredMismatch`) on every determination. The signer must scale by 1e7 on Stellar and
//! keep 1e6 on Base.
//!
//! ## Necessary changes vs EVM (documented for the Lit PKP team)
//! - `block.chainid` (u256) -> `network_domain: BytesN<32>` (bind the Stellar network passphrase).
//! - `address(this)` (20B) -> the contract `Address` serialized via its canonical XDR bytes
//!   (Soroban addresses are not 20-byte EVM addresses).
//! - Signature is 64-byte `r‖s` + a separate `recovery_id (0/1)`, recovered with
//!   `secp256k1_recover`; the authorized signer is stored/compared as the 65-byte SEC-1 pubkey.

use soroban_sdk::crypto::Hash;
use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{Address, Bytes, BytesN, Env};

use crate::types::{CropDetermination, DroughtDetermination};

/// `keccak256(bytes("1.0"))` domain string (schema version).
pub const SCHEMA_VERSION: &[u8] = b"1.0";
/// `keccak256(bytes("CROP_DAMAGE"))` domain string (determination kind).
pub const KIND_CROP: &[u8] = b"CROP_DAMAGE";
/// `keccak256(bytes("crop-dualindex-1.0"))` domain string (pins the 60/40 weights).
pub const METHODOLOGY_CROP: &[u8] = b"crop-dualindex-1.0";

/// `keccak256(bytes("CROP_DROUGHT"))` domain string (drought determination kind).
pub const KIND_DROUGHT: &[u8] = b"CROP_DROUGHT";
/// `keccak256(bytes("crop-drought-rdi-1.0"))` domain string (pins the rdi-1.0 index).
pub const METHODOLOGY_DROUGHT_RDI_1_0: &[u8] = b"crop-drought-rdi-1.0";

/// The exact bytes of `DROUGHT_PARAMS_JSON` in `microcrop-lit-oracle`
/// `src/scoring/drought.shared.js` (origin/main `1bc2d9d`). Off-chain reference only; the
/// contract compares the signed hash with [`DROUGHT_PARAMS_HASH_RDI_1_0`].
pub const DROUGHT_PARAMS_JSON: &str = concat!(
    r#"{"model":"era5","tz":"Africa/Nairobi","climFrom":1991,"climTo":2020,"#,
    r#""minSeasons":30,"lgp":180,"w1":[1,40],"w2":[71,110],"ky1_e2":40,"ky2_e2":150,"#,
    r#""dryDay_e2":100,"agg":"mult","scale":"e2"}"#
);

/// `keccak256(DROUGHT_PARAMS_JSON)` =
/// `0x2580afa0002f8ae349ad078ffea429049ab1c1146be2c56b82f3e45a7cc447cf`, computed by the
/// oracle's own code (ethers v6 `keccak256(toUtf8Bytes(DROUGHT_PARAMS_JSON))`). The EVM
/// fixture's `0x22ff…` is an arbitrary test value, not this one.
pub const DROUGHT_PARAMS_HASH_RDI_1_0: [u8; 32] = [
    0x25, 0x80, 0xaf, 0xa0, 0x00, 0x2f, 0x8a, 0xe3, 0x49, 0xad, 0x07, 0x8f, 0xfe, 0xa4, 0x29, 0x04,
    0x9a, 0xb1, 0xc1, 0x14, 0x6b, 0xe2, 0xc5, 0x6b, 0x82, 0xf3, 0xe4, 0x5a, 0x7c, 0xc4, 0x47, 0xcf,
];

/// Byte length of [`encode_drought_inputs`]: 13 words.
pub const DROUGHT_INPUTS_LEN: u32 = 13 * 32;
/// Byte length of [`encode_drought_preimage`]: 3 domain words, network domain, 40-byte
/// contract XDR, inputs hash, 5 result words.
pub const DROUGHT_PREIMAGE_LEN: u32 = 3 * 32 + 32 + 40 + 32 + 5 * 32;

/// 32-byte big-endian, two's-complement word for a signed value.
fn word_from_i128(v: i128) -> [u8; 32] {
    let mut w = [0u8; 32];
    if v < 0 {
        // sign-extend the high 16 bytes
        let mut i = 0;
        while i < 16 {
            w[i] = 0xff;
            i += 1;
        }
    }
    let be = v.to_be_bytes(); // 16 bytes, two's complement
    let mut i = 0;
    while i < 16 {
        w[16 + i] = be[i];
        i += 1;
    }
    w
}

/// 32-byte big-endian word for an unsigned value (zero-padded high 16 bytes).
fn word_from_u128(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    let be = v.to_be_bytes();
    let mut i = 0;
    while i < 16 {
        w[16 + i] = be[i];
        i += 1;
    }
    w
}

fn keccak_of(env: &Env, s: &[u8]) -> [u8; 32] {
    env.crypto()
        .keccak256(&Bytes::from_slice(env, s))
        .to_array()
}

/// Serialize the 10 evidence fields (the `inputsHash` preimage). Field order is FROZEN.
pub fn encode_inputs(env: &Env, d: &CropDetermination) -> Bytes {
    let mut b = Bytes::new(env);
    b.extend_from_array(&word_from_u128(d.on_chain_policy_id as u128));
    b.extend_from_array(&word_from_i128(d.latitude_e6));
    b.extend_from_array(&word_from_i128(d.longitude_e6));
    b.extend_from_array(&word_from_i128(d.sum_insured));
    b.extend_from_array(&word_from_i128(d.ndvi_scaled));
    b.extend_from_array(&word_from_u128(d.weather_present as u128));
    b.extend_from_array(&word_from_i128(d.weather_temp_c_e2));
    b.extend_from_array(&word_from_u128(d.weather_precip_e2));
    b.extend_from_array(&word_from_u128(d.weather_humidity));
    b.extend_from_array(&word_from_u128(d.weather_wind_e2));
    b
}

/// Serialize the settlement preimage. `contract` binds `address(this)`; `network_domain`
/// binds the network (replaces `block.chainid`) — together they give cross-network /
/// cross-contract replay protection. Field order is FROZEN.
pub fn encode_preimage(
    env: &Env,
    inputs_hash: &BytesN<32>,
    d: &CropDetermination,
    contract: &Address,
    network_domain: &BytesN<32>,
) -> Bytes {
    let mut b = Bytes::new(env);
    b.extend_from_array(&keccak_of(env, SCHEMA_VERSION));
    b.extend_from_array(&keccak_of(env, KIND_CROP));
    b.extend_from_array(&keccak_of(env, METHODOLOGY_CROP));
    b.extend_from_array(&network_domain.to_array());
    // contract identity — canonical XDR serialization of the Address
    b.append(&contract.clone().to_xdr(env));
    b.extend_from_array(&inputs_hash.to_array());
    b.extend_from_array(&word_from_u128(d.on_chain_policy_id as u128));
    b.extend_from_array(&word_from_u128(d.damage_percent_bp as u128));
    b.extend_from_array(&word_from_u128(d.weather_damage as u128));
    b.extend_from_array(&word_from_u128(d.satellite_damage as u128));
    b.extend_from_array(&word_from_i128(d.payout_amount));
    b.extend_from_array(&word_from_u128(d.assessed_at as u128));
    b
}

/// Reconstruct the full settlement digest on-chain from raw fields — never trust a
/// passed-in hash. The returned `Hash<32>` is BOTH the `secp256k1_recover` message digest
/// and (via `.to_bytes()`) the replay key.
pub fn determination_digest(
    env: &Env,
    d: &CropDetermination,
    contract: &Address,
    network_domain: &BytesN<32>,
) -> Hash<32> {
    let inputs = encode_inputs(env, d);
    let inputs_hash = env.crypto().keccak256(&inputs).to_bytes();
    let preimage = encode_preimage(env, &inputs_hash, d, contract, network_domain);
    env.crypto().keccak256(&preimage)
}

// ============ CROP_DROUGHT (`crop-drought-rdi-1.0`) ============

/// Serialize the 13 drought evidence words (the drought `inputsHash` preimage, 416 bytes).
/// Field order is FROZEN and byte-identical to the EVM `abi.encode` of
/// `PayoutReceiver.sol` v2.2.0 (only the sum-insured scale differs: 7dp here):
///   on_chain_policy_id, latitude_e6, longitude_e6, sum_insured, season_start_epoch,
///   lgp_days, rain_w1_e2, rain_ref_w1_e2, rain_w2_e2, rain_ref_w2_e2, cdd_w2_days,
///   source_hash (raw 32 bytes), methodology_params_hash (raw 32 bytes)
pub fn encode_drought_inputs(env: &Env, d: &DroughtDetermination) -> Bytes {
    let mut b = Bytes::new(env);
    b.extend_from_array(&word_from_u128(d.on_chain_policy_id as u128));
    b.extend_from_array(&word_from_i128(d.latitude_e6));
    b.extend_from_array(&word_from_i128(d.longitude_e6));
    b.extend_from_array(&word_from_i128(d.sum_insured));
    b.extend_from_array(&word_from_u128(d.season_start_epoch as u128));
    b.extend_from_array(&word_from_u128(d.lgp_days as u128));
    b.extend_from_array(&word_from_u128(d.rain_w1_e2));
    b.extend_from_array(&word_from_u128(d.rain_ref_w1_e2));
    b.extend_from_array(&word_from_u128(d.rain_w2_e2));
    b.extend_from_array(&word_from_u128(d.rain_ref_w2_e2));
    b.extend_from_array(&word_from_u128(d.cdd_w2_days as u128));
    b.extend_from_array(&d.source_hash.to_array());
    b.extend_from_array(&d.methodology_params_hash.to_array());
    b
}

/// Serialize the drought settlement preimage (360 bytes). Field order is FROZEN:
///
/// | bytes | content |
/// |---|---|
/// | 0..32 | `keccak256("1.0")` |
/// | 32..64 | `keccak256("CROP_DROUGHT")` |
/// | 64..96 | `keccak256("crop-drought-rdi-1.0")` |
/// | 96..128 | network domain (`sha256(network passphrase)`) |
/// | 128..168 | `contract.to_xdr()` = `00000012 00000001` ‖ 32-byte contract id |
/// | 168..200 | `keccak256(encode_drought_inputs)` |
/// | 200..360 | words: on_chain_policy_id, damage_percent_bp, drought_damage, payout_amount, assessed_at |
///
/// Domain separation: the CROP_DAMAGE preimage is 392 bytes with `keccak256("CROP_DAMAGE")`
/// at 32..64; the EVM drought preimage is 340 bytes packed (chain id and a 20-byte address);
/// canonical determinations are SHA-256 over JSON text. Lengths, kind words and domain words
/// all differ, so no digest can serve two paths.
pub fn encode_drought_preimage(
    env: &Env,
    inputs_hash: &BytesN<32>,
    d: &DroughtDetermination,
    contract: &Address,
    network_domain: &BytesN<32>,
) -> Bytes {
    let mut b = Bytes::new(env);
    b.extend_from_array(&keccak_of(env, SCHEMA_VERSION));
    b.extend_from_array(&keccak_of(env, KIND_DROUGHT));
    b.extend_from_array(&keccak_of(env, METHODOLOGY_DROUGHT_RDI_1_0));
    b.extend_from_array(&network_domain.to_array());
    b.append(&contract.clone().to_xdr(env));
    b.extend_from_array(&inputs_hash.to_array());
    b.extend_from_array(&word_from_u128(d.on_chain_policy_id as u128));
    b.extend_from_array(&word_from_u128(d.damage_percent_bp as u128));
    b.extend_from_array(&word_from_u128(d.drought_damage as u128));
    b.extend_from_array(&word_from_i128(d.payout_amount));
    b.extend_from_array(&word_from_u128(d.assessed_at as u128));
    b
}

/// The drought settlement digest: `keccak256(encode_drought_preimage(keccak256(inputs)))`.
/// Both the `secp256k1_recover` message and the replay key.
pub fn drought_determination_digest(
    env: &Env,
    d: &DroughtDetermination,
    contract: &Address,
    network_domain: &BytesN<32>,
) -> Hash<32> {
    let inputs = encode_drought_inputs(env, d);
    let inputs_hash = env.crypto().keccak256(&inputs).to_bytes();
    let preimage = encode_drought_preimage(env, &inputs_hash, d, contract, network_domain);
    env.crypto().keccak256(&preimage)
}

#[cfg(test)]
mod tests;
