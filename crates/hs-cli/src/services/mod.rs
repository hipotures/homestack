//! Services an application repository declares in `.homestack/*.yaml`
//! (manifest schema 1): the parser, their discovery on GitHub and the
//! profile's registry of what HomeStack deployed.

pub mod github;
pub mod manifest;
pub mod registry;
