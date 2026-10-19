#![no_std]
//! # microcrop-shared
//!
//! Cross-cutting building blocks shared by all four MicroCrop Soroban contracts.
//! Keeping these in one rlib guarantees the parallel per-contract implementations
//! agree on the wire-level details that MUST match exactly:
//!
//! - [`types`]      — `#[contracttype]` structs/enums ported 1:1 from the Solidity structs.
//! - [`errors`]     — `#[contracterror]` codes (one enum per contract domain + a common one).
//! - [`roles`]      — `AccessControl`-equivalent role registry + `require_role` guard.
//! - [`interfaces`] — `#[contractclient]` traits for cross-contract calls (no WASM build coupling).
//! - [`encoding`]   — the FROZEN canonical byte layouts for the PKP determination signatures.
//! - [`drought`]    — the `crop-drought-rdi-1.0` index recompute and evidence bounds.
//! - [`ttl`]        — storage TTL policy: the rent cap (bounded user-path and keeper targets).

pub mod drought;
pub mod encoding;
pub mod errors;
pub mod interfaces;
pub mod roles;
pub mod ttl;
pub mod types;
