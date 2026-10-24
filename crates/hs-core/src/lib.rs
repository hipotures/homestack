//! HomeStack operations: plans, state machines and verification.
//!
//! The logic is written against [`pve::Pve`], so it runs the same on the real
//! client and on test doubles. Each function ports a Python function and
//! names it; behaviour and messages follow Python unless
//! `docs/migration/phase-N.md` records a difference.

pub mod addkey;
pub mod capabilities;
mod capacity;
pub mod cloudinit;
pub mod create_transfer;
pub mod discovery;
pub mod error;
pub mod gold;
pub mod guest;
pub mod inventory;
pub mod keys;
pub mod lease;
pub mod lifecycle;
pub mod migrate;
pub mod power;
pub mod prefetch;
pub mod pve;
pub mod refresh;
pub mod registry;
pub mod resize;
pub mod resize_guest;
pub mod service_status;
pub mod status;
pub mod tasks;
pub mod verify;
mod workspace_destroy;

pub use error::CoreError;
