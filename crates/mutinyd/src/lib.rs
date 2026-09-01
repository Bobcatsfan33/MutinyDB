//! `mutinyd` — one process, one admission boundary, three doors (M6, `docs/M6-SURFACE.md`).
//!
//! This crate is the distributable v0.1 developer release. All four linked components are
//! release-admitted. Production approval remains a separate gate until an external-KMS custody
//! receipt exists (MD-7).

pub mod config;
pub mod fleet;
pub mod mcp;
pub mod metrics;
pub mod plane;
pub mod server;

pub use config::{Config, ConfigError, QUARANTINE_NOTICE, RELEASE_NOTICE, SURFACE_VERSION};
pub use metrics::Metrics;
pub use plane::{PlaneError, TenantPlane, WriteRequest};
pub use server::{banner, MutinyServer};

/// The action every merged standing write is re-evaluated as (Loom AT-016, composed).
pub const MERGE_ACTION: &str = "standing.merge";
