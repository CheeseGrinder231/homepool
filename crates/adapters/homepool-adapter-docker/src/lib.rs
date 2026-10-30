//! Synchronous Docker CLI adapter for explicitly selected local or SSH engines.
//! Requires the Docker CLI and Unix `/dev/stdin`. Operations are scoped to one
//! core member identity. There is no placement, membership, or slot policy here.
//!
//! All environment values travel over stdin. They remain visible to Docker
//! administrators in container configuration. Other spec fields must be nonsecret.

mod adapter;
mod command;
mod endpoint;
mod error;
mod limits;
mod model;
mod settings;
mod spec;

pub use adapter::Docker;
pub use endpoint::Endpoint;
pub use error::Error;
pub use model::{BindMount, EngineCapacity, Environment, Sandbox, SandboxSpec};

#[cfg(test)]
mod tests;

#[cfg(all(test, target_os = "linux"))]
mod deadline_tests;
