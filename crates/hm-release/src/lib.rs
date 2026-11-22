//! Home Mate release tool: manifest verification, package install, and the
//! health-gated A/B activation. `main.rs` is a thin CLI over this library;
//! host integration tests use the same code path the device runs.

pub mod activate;
pub mod corebin;
pub mod crypto;
pub mod device;
pub mod health;
pub mod hexutil;
pub mod http;
pub mod install;
pub mod manifest;
pub mod pack;
pub mod package;
pub mod state;
