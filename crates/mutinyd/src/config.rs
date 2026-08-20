//! `mutinyd`'s configuration: one JSON file, validated loudly at startup. A server that guesses a
//! default for a missing tenant field is a server whose behavior nobody configured.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The surface version every door reports (docs/M6-SURFACE.md's versioning promise).
pub const SURFACE_VERSION: &str = "v0";

/// The statement `--help` and `/health` carry (MD-6's quarantine, said out loud).
pub const QUARANTINE_NOTICE: &str = "composed-development build: every linked component is \
     release-quarantined (components.lock.json); NOT a supported or distributable artifact until \
     M8's release gates clear";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `host:port` to bind. Tests use `127.0.0.1:0` and read the bound port.
    pub listen: String,
    /// The operator bearer token gating execute/taint/shutdown. Possession is the wire form of
    /// M3's agent/operator type separation.
    pub operator_token: String,
    /// Root under which each tenant gets `<name>/storage` and `<name>/compute`.
    pub data_dir: PathBuf,
    #[serde(default = "default_checkpoint_every")]
    pub checkpoint_every: u64,
    pub embedding: EmbeddingConfig,
    pub tenants: Vec<TenantConfig>,
}

fn default_checkpoint_every() -> u64 {
    8
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingConfig {
    pub dim: usize,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub name: String,
    #[serde(default)]
    pub quota: QuotaConfig,
    pub tables: Vec<TableConfig>,
    #[serde(default)]
    pub semantic_standing: SemanticStandingConfig,
    /// v0 connectors: deterministic echo connectors (`docs/M6-SURFACE.md`). Real integrations are
    /// exactly the work this field will grow into.
    #[serde(default)]
    pub connectors: Vec<ConnectorConfig>,
    /// Awake maintenance triggers after this many commits (docs/M8-MAINTENANCE.md, issue #12).
    /// The default is the measured constant in `evidence/m8-maintenance-policy.json`, not
    /// folklore. `0` disables — dev only; the nightly soak gates the default.
    #[serde(default = "default_maintenance_every")]
    pub maintenance_every: u64,
}

/// See `crates/mutinyd/evidence/m8-maintenance-policy.json` for the measurement that chose this.
pub fn default_maintenance_every() -> u64 {
    64
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaConfig {
    /// Requests admitted per rolling one-second window (Prism's windowed quota discipline).
    pub requests_per_sec: u64,
    /// Request bytes admitted per window.
    pub bytes_per_sec: u64,
    /// The tenant's bounded queue depth; a full queue is `Overloaded`, the retryable kind.
    pub queue_depth: usize,
}

impl Default for QuotaConfig {
    fn default() -> QuotaConfig {
        QuotaConfig {
            requests_per_sec: 1_000,
            bytes_per_sec: 8 * 1024 * 1024,
            queue_depth: 64,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TableConfig {
    pub name: String,
    /// `[name, type]` pairs; types are `utf8`, `int64`, `bool`.
    pub columns: Vec<(String, String)>,
    pub key_column: String,
    pub branch_column: String,
    /// The channel plane segment (MD-2 R6): `<tenant>/<plane>/<table>`.
    pub plane: String,
    /// If present, rows of this table feed the branch-scoped semantic operators.
    #[serde(default)]
    pub semantic: Option<SemanticColumnsConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticColumnsConfig {
    pub body_column: String,
    pub event_time_column: String,
    pub cost_micros_column: String,
    pub error_column: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticStandingConfig {
    #[serde(default)]
    pub topk: Vec<TopKConfig>,
    #[serde(default)]
    pub groups: Vec<GroupsConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopKConfig {
    pub id: String,
    pub text: String,
    pub k: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupsConfig {
    pub id: String,
    pub anchors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    pub action_type: String,
    pub compensating_action: Option<String>,
    /// Receipt prefix; the echo connector answers `<receipt_prefix>:<target>`.
    pub receipt_prefix: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the config could not be read: {0}")]
    Io(String),
    #[error("the config could not be parsed: {0}")]
    Parse(String),
    #[error("config: {0}")]
    Invalid(String),
}

impl Config {
    pub fn from_path(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        Config::from_json(&text)
    }

    pub fn from_json(text: &str) -> Result<Config, ConfigError> {
        let config: Config =
            serde_json::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let fail = |message: String| Err(ConfigError::Invalid(message));
        if self.tenants.is_empty() {
            return fail("at least one tenant is required".to_owned());
        }
        if self.operator_token.trim().is_empty() {
            return fail("operator_token must be non-empty".to_owned());
        }
        let mut names = BTreeMap::new();
        for tenant in &self.tenants {
            if names.insert(tenant.name.clone(), ()).is_some() {
                return fail(format!("tenant {:?} is declared twice", tenant.name));
            }
            validate_tenant(tenant)?;
        }
        Ok(())
    }
}

/// Per-tenant validation, shared by the static config and the fleet's dynamic registration.
pub fn validate_tenant(tenant: &TenantConfig) -> Result<(), ConfigError> {
    let fail = |message: String| Err(ConfigError::Invalid(message));
    {
        {
            if tenant.name.trim().is_empty()
                || tenant.name.contains('/')
                || tenant.name.contains("..")
            {
                return fail(format!(
                    "tenant name {:?} is not a safe identifier",
                    tenant.name
                ));
            }
            if tenant.tables.is_empty() {
                return fail(format!("tenant {:?} declares no tables", tenant.name));
            }
            for table in &tenant.tables {
                let column = |name: &str| table.columns.iter().any(|(c, _)| c == name);
                if !column(&table.key_column) || !column(&table.branch_column) {
                    return fail(format!(
                        "table {}.{}: key_column and branch_column must name declared columns",
                        tenant.name, table.name
                    ));
                }
                for (name, kind) in &table.columns {
                    if !matches!(kind.as_str(), "utf8" | "int64" | "bool") {
                        return fail(format!(
                            "table {}.{} column {name:?}: unknown type {kind:?} (utf8|int64|bool)",
                            tenant.name, table.name
                        ));
                    }
                }
                if let Some(semantic) = &table.semantic {
                    for required in [
                        &semantic.body_column,
                        &semantic.event_time_column,
                        &semantic.cost_micros_column,
                        &semantic.error_column,
                    ] {
                        if !column(required) {
                            return fail(format!(
                                "table {}.{}: semantic column {required:?} is not declared",
                                tenant.name, table.name
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
