//! HomeStack operations: plans, state machines and verification.
//!
//! The logic is written against a Proxmox abstraction so it runs the same on
//! the real client and on the simulator. Phase 1 adds the privileges each
//! operation needs ([`capabilities`]); the operations themselves arrive from
//! phase 2 of `docs/migration/PLAN.md`.

pub mod capabilities;
