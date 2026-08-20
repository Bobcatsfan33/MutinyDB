//! `mutinyd` — one process, one admission boundary, three doors (M6, `docs/M6-SURFACE.md`).
//!
//! The quarantine notice governs: every linked component is release-quarantined, so this is the
//! composed-development form of the product binary; it becomes a supported artifact only at M8.

pub mod config;
pub mod fleet;
pub mod mcp;
pub mod metrics;
pub mod plane;
pub mod server;

pub use config::{Config, ConfigError, QUARANTINE_NOTICE, SURFACE_VERSION};
pub use metrics::Metrics;
pub use plane::{PlaneError, TenantPlane, WriteRequest};
pub use server::{banner, MutinyServer};

/// The action every merged standing write is re-evaluated as (Loom AT-016, composed).
pub const MERGE_ACTION: &str = "standing.merge";
