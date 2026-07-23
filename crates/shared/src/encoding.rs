//! FROZEN canonical byte layout for the PKP determination signature.
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
//! ## Necessary changes vs EVM (documented for the Lit PKP team)
//! - `block.chainid` (u256) -> `network_domain: BytesN<32>` (bind the Stellar network passphrase).
//! - `address(this)` (20B) -> the contract `Address` serialized via its canonical XDR bytes
//!   (Soroban addresses are not 20-byte EVM addresses).
//! - Signature is 64-byte `r‖s` + a separate `recovery_id (0/1)`, recovered with
//!   `secp256k1_recover`; the authorized signer is stored/compared as the 65-byte SEC-1 pubkey.

use soroban_sdk::crypto::Hash;
use soroban_sdk::xdr::ToXdr;
use soroban_sdk::{Address, Bytes, BytesN, Env};

use crate::types::CropDetermination;

/// `keccak256(bytes("1.0"))` domain string (schema version).
pub const SCHEMA_VERSION: &[u8] = b"1.0";
/// `keccak256(bytes("CROP_DAMAGE"))` domain string (determination kind).
pub const KIND_CROP: &[u8] = b"CROP_DAMAGE";
/// `keccak256(bytes("crop-dualindex-1.0"))` domain string (pins the 60/40 weights).
pub const METHODOLOGY_CROP: &[u8] = b"crop-dualindex-1.0";

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
