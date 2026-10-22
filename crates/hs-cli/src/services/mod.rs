//! Services an application repository declares in `.homestack/*.yaml`
//! (manifest schema 1): the parser, their discovery on GitHub, their first
//! deployment and the profile's registry of what HomeStack deployed.

pub mod deploy;
pub mod github;
pub mod manifest;
pub mod registry;

pub mod lifecycle;
pub mod replacement;
