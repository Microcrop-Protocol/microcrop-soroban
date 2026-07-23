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
//! - [`encoding`]   — the FROZEN canonical byte layout for the PKP determination signature.

pub mod encoding;
pub mod errors;
pub mod interfaces;
pub mod roles;
pub mod types;
