//! The fleet registry (docs/M7-FLEET.md): the durable truth about which tenants exist and how
//! they may be woken. One JSON file under the data root, written atomically; the static config's
//! tenant list seeds it on first boot, and thereafter the registry is authoritative.

use crate::config::{Config, TenantConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const REGISTRY_FILE: &str = "fleet-registry.json";

/// How a registered, non-resident tenant may be brought back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    /// The tenant was awake (or has never slept). Waking it takes the full-replay crash path —
    /// correct, O(history), and exactly what the kill matrix gates.
    Awake,
    /// The tenant was slept through the contract: drain → compact → plane checkpoint. Waking it
    /// takes the bounded path, and a missing checkpoint is refused by name, never silently
    /// replayed around.
    Asleep,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FleetRow {
    pub config: TenantConfig,
    pub state: RowState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FleetRegistry {
    pub rows: BTreeMap<String, FleetRow>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    #[error("fleet registry: {0}")]
    Io(String),
    #[error("fleet registry: {0}")]
    Corrupt(String),
}

impl FleetRegistry {
    /// Load the registry, or seed it from the static config's tenant list on first boot.
    pub fn load_or_seed(
        data_dir: &std::path::Path,
        config: &Config,
    ) -> Result<FleetRegistry, FleetError> {
        std::fs::create_dir_all(data_dir).map_err(|e| FleetError::Io(e.to_string()))?;
        let path = data_dir.join(REGISTRY_FILE);
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|e| FleetError::Io(e.to_string()))?;
            let mut registry: FleetRegistry =
                serde_json::from_str(&text).map_err(|e| FleetError::Corrupt(e.to_string()))?;
            registry.path = path;
            return Ok(registry);
        }
        let mut registry = FleetRegistry {
            rows: BTreeMap::new(),
            path,
        };
        for tenant in &config.tenants {
            registry.rows.insert(
                tenant.name.clone(),
                FleetRow {
                    config: tenant.clone(),
                    state: RowState::Awake,
                },
            );
        }
        registry.save()?;
        Ok(registry)
    }

    /// Atomic persist: tmp + rename, so a crash never leaves a half-written fleet truth.
    pub fn save(&self) -> Result<(), FleetError> {
        let text =
            serde_json::to_string_pretty(self).map_err(|e| FleetError::Corrupt(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| FleetError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| FleetError::Io(e.to_string()))?;
        Ok(())
    }
}
