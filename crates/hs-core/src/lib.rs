//! HomeStack operations: plans, state machines and verification.
//!
//! The logic is written against [`pve::Pve`], so it runs the same on the real
//! client and on test doubles. Each function ports a Python function and
//! names it; behaviour and messages follow Python unless
//! `docs/migration/phase-N.md` records a difference.

pub mod capabilities;
pub mod discovery;
pub mod error;
pub mod gold;
pub mod inventory;
pub mod pve;
pub mod status;

pub use error::CoreError;
