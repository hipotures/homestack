//! HomeStack operations: plans, state machines and verification.
//!
//! The logic is written against a Proxmox abstraction so it runs the same on
//! the real client and on the simulator. It arrives with the first migrated
//! operations (phase 2 of `docs/migration/PLAN.md`); phase 0 only reserves
//! the crate in the workspace layout.
