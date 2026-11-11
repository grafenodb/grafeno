//! `grafeno.toml`: the settings of an installed server, shared by
//! `grafeno serve --config`, the system service and `grafeno shell --config`.
//!
//! ```toml
//! db = "/var/lib/grafeno/grafeno.redb"
//! addr = "127.0.0.1:7810"
//! token = "…"            # every /v1 request needs `Authorization: Bearer <token>`
//! cors_origins = []      # origins allowed to call the API from a browser
//! durability = "immediate"  # or "eventual": commits reach the disk within 0.5 s
//! max_requests = 512     # served at once; more get 503 with Retry-After
//! copy_before_upgrade = true  # copy the file before a new version upgrades its layout
//!
//! [query]                # limits for each GQ query; 0 turns one off
//! timeout_secs = 30
//! max_reads = 50000000   # nodes, edges and index entries read
//! max_rows = 1000000     # rows held at once
//! max_memory_mb = 512    # one value built, or the rows held at once
//! max_cost = 0           # refuse queries EXPLAIN estimates above this (off)
//! scan_threads = 0       # threads large scans may share; 0 one per CPU, 1 none
//!
//! [writer]               # the group commit
//! max_batch = 512        # writes per transaction at most
//! queue = 4096           # writes that may wait; more get 503
//! flush_ms = 500         # with eventual durability, how often commits reach the disk
//! import_chunk = 2000    # records per transaction of a bulk import
//!
//! [memory]               # caches, in MiB
//! cache_mb = 256         # pages of the file
//! projection_cache_mb = 256  # graphs kept for algorithms
//! vector_cache_mb = 128  # vectors decoded while inserting
//!
//! [changes]              # the change log, per namespace
//! enabled = true
//! max_entries = 1000000  # 0 for no limit
//! max_age_days = 7       # 0 for no limit
//!
//! [events]               # change events with values (16.10)
//! enabled = false        # subscriptions record values when true
//!
//! [auth]                 # accounts and sign-in (14.1)
//! session_minutes = 15   # an access token's life
//! refresh_days = 30      # a session's life, refreshed or not
//! lockout_attempts = 5   # failed sign-ins that lock an account
//! address_attempts = 20  # failed sign-ins that refuse an address
//! lockout_minutes = 15   # how long both last
//!
//! [cluster]              # several servers kept in sync by Raft (13.2)
//! id = 1
//! addr = "10.0.0.1:7811" # where members talk to each other
//! secret = "…"           # the same on every member
//! voters = [1, 2, 3]     # the first members ([] when joining)
//! peers = [{ id = 1, addr = "10.0.0.1:7811" }, { id = 2, addr = "10.0.0.2:7811" }]
//!
//! [background]           # work that runs from the change log
//! extract = ["notes"]    # extract mentions as texts change (11.1)
//! communities = ["notes"]  # keep community summaries fresh (11.3)
//! communities_every_secs = 300
//! communities_min_changes = 50
//!
//! [transactions]         # explicit transactions (16.4)
//! idle_timeout_secs = 30 # rolled back after this long without a request
//! max_secs = 600         # rolled back at this age
//! wait_secs = 5          # how long BEGIN waits for the writing one before
//! max_open = 256
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7810";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Ignored since Grafeno 15: the engine serves no interface (Grafeno
    /// Studio is installed apart). Kept so older settings still load.
    #[serde(default, skip_serializing)]
    pub studio: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cors_origins: Vec<String>,
    /// `immediate` (the default) or `eventual`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<DurabilitySetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<usize>,
    /// Keep a copy of the database before a new version upgrades its
    /// layout (true by default; see `Grafeno::copy_before_upgrade`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_before_upgrade: Option<bool>,
    #[serde(default, skip_serializing_if = "QueryConfig::is_empty")]
    pub query: QueryConfig,
    #[serde(default, skip_serializing_if = "MemoryConfig::is_empty")]
    pub memory: MemoryConfig,
    #[serde(default, skip_serializing_if = "WriterConfig::is_empty")]
    pub writer: WriterConfig,
    #[serde(default, skip_serializing_if = "ChangesConfig::is_empty")]
    pub changes: ChangesConfig,
    /// Change events with values (16.10), off by default.
    #[serde(default, skip_serializing_if = "EventsConfig::is_empty")]
    pub events: EventsConfig,
    /// How much of the audit log is kept (14.5).
    #[serde(default, skip_serializing_if = "AuditConfig::is_empty")]
    pub audit: AuditConfig,
    #[serde(
        default,
        skip_serializing_if = "crate::events::BackgroundConfig::is_empty"
    )]
    pub background: crate::events::BackgroundConfig,
    /// Explicit transactions (16.4).
    #[serde(default, skip_serializing_if = "TransactionsConfig::is_empty")]
    pub transactions: TransactionsConfig,
    /// Membership of a cluster (phase 13.2); absent for a single server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<crate::cluster::ClusterConfig>,
    /// Sessions and lockouts (14.1).
    #[serde(default, skip_serializing_if = "AuthSection::is_empty")]
    pub auth: AuthSection,
}

/// `[auth]`: missing values take [`crate::accounts::AuthConfig`]'s defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_days: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockout_attempts: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address_attempts: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockout_minutes: Option<u64>,
}

impl AuthSection {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn config(&self) -> crate::accounts::AuthConfig {
        let default = crate::accounts::AuthConfig::default();
        let minutes = |m: u64| std::time::Duration::from_secs(m.max(1) * 60);
        crate::accounts::AuthConfig {
            session: self.session_minutes.map_or(default.session, minutes),
            refresh: self.refresh_days.map_or(default.refresh, |d| {
                std::time::Duration::from_secs(d.max(1) * 24 * 3600)
            }),
            lockout: crate::auth::Lockout {
                account_attempts: self
                    .lockout_attempts
                    .unwrap_or(default.lockout.account_attempts)
                    .max(1),
                address_attempts: self
                    .address_attempts
                    .unwrap_or(default.lockout.address_attempts)
                    .max(1),
                window: self.lockout_minutes.map_or(default.lockout.window, minutes),
            },
        }
    }
}

/// How long explicit transactions may live and wait (16.4); missing values
/// take [`crate::transactions::TransactionSettings`]'s defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransactionsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_open: Option<usize>,
}

impl TransactionsConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn settings(&self) -> crate::transactions::TransactionSettings {
        let base = crate::transactions::TransactionSettings::default();
        let secs = |value: Option<f64>, default: std::time::Duration| {
            value
                .filter(|s| s.is_finite() && *s > 0.0)
                .map_or(default, std::time::Duration::from_secs_f64)
        };
        crate::transactions::TransactionSettings {
            idle: secs(self.idle_timeout_secs, base.idle),
            lifetime: secs(self.max_secs, base.lifetime),
            wait: secs(self.wait_secs, base.wait),
            max_open: self.max_open.filter(|n| *n > 0).unwrap_or(base.max_open),
        }
    }
}

/// How much of the change log is kept; missing values take
/// [`grafeno::Retention`]'s defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<u64>,
}

impl ChangesConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn retention(&self) -> grafeno::Retention {
        let base = grafeno::Retention::default();
        grafeno::Retention {
            enabled: self.enabled.unwrap_or(base.enabled),
            max_entries: self.max_entries.unwrap_or(base.max_entries),
            max_age_secs: self
                .max_age_days
                .map_or(base.max_age_secs, |days| days.saturating_mul(86_400)),
        }
    }
}

/// `[events]`: change events with values (16.10). Off unless `enabled`;
/// while off, no subscription can be created and writes record nothing
/// more than the change log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl EventsConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }
}

/// `[audit]`: how much of the audit log is kept; missing values take
/// [`grafeno::AuditRetention`]'s defaults (ten million events, a year).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age_days: Option<u64>,
}

impl AuditConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn retention(&self) -> grafeno::AuditRetention {
        let base = grafeno::AuditRetention::default();
        grafeno::AuditRetention {
            max_entries: self.max_entries.unwrap_or(base.max_entries),
            max_age_secs: self
                .max_age_days
                .map_or(base.max_age_secs, |days| days.saturating_mul(86_400)),
        }
    }
}

/// The group-commit writer; missing values take
/// [`grafeno::WriterOptions`]'s defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flush_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_chunk: Option<usize>,
}

impl WriterConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn options(&self) -> grafeno::WriterOptions {
        let base = grafeno::WriterOptions::default();
        grafeno::WriterOptions {
            max_batch: self.max_batch.unwrap_or(base.max_batch).max(1),
            queue: self.queue.unwrap_or(base.queue).max(1),
            flush_every: self.flush_ms.map_or(base.flush_every, |ms| {
                std::time::Duration::from_millis(ms.max(1))
            }),
            ..base
        }
    }

    pub fn import_chunk(&self) -> usize {
        self.import_chunk
            .unwrap_or(crate::http::IMPORT_CHUNK)
            .max(1)
    }
}

/// Cache sizes, in MiB; missing values take [`grafeno::OpenOptions`]'s
/// defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_cache_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_cache_mb: Option<u64>,
}

impl MemoryConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn options(&self) -> grafeno::OpenOptions {
        let base = grafeno::OpenOptions::default();
        let bytes = |mb: Option<u64>, default: usize| {
            mb.map_or(default, |mb| {
                usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX)
            })
        };
        grafeno::OpenOptions {
            cache_bytes: bytes(self.cache_mb, base.cache_bytes),
            projection_cache_bytes: bytes(self.projection_cache_mb, base.projection_cache_bytes),
            vector_cache_bytes: bytes(self.vector_cache_mb, base.vector_cache_bytes),
            ..base
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurabilitySetting {
    Immediate,
    Eventual,
}

impl From<DurabilitySetting> for grafeno::Durability {
    fn from(setting: DurabilitySetting) -> Self {
        match setting {
            DurabilitySetting::Immediate => grafeno::Durability::Immediate,
            DurabilitySetting::Eventual => grafeno::Durability::Eventual,
        }
    }
}

/// Limits for each GQ query over HTTP and MCP. Missing values take the
/// defaults of [`grafeno_query::Limits::standard`]; 0 turns a limit off.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reads: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    /// Threads parallel scans may use at once, over every query: 0 is one
    /// per CPU, 1 turns them off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_threads: Option<usize>,
}

impl QueryConfig {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn limits(&self) -> grafeno_query::Limits {
        let standard = grafeno_query::Limits::standard();
        grafeno_query::Limits {
            timeout: match self.timeout_secs {
                Some(secs) if secs > 0.0 => Some(std::time::Duration::from_secs_f64(secs)),
                Some(_) => None,
                None => standard.timeout,
            },
            max_reads: match self.max_reads {
                Some(0) => None,
                Some(max) => Some(max),
                None => standard.max_reads,
            },
            max_rows: match self.max_rows {
                Some(0) => None,
                Some(max) => Some(max),
                None => standard.max_rows,
            },
            max_memory: match self.max_memory_mb {
                Some(0) => None,
                Some(mb) => {
                    Some(usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX))
                }
                None => standard.max_memory,
            },
            max_cost: match self.max_cost {
                Some(max) if max > 0.0 => Some(max),
                Some(_) => None,
                None => standard.max_cost,
            },
            cancel: None,
            visible: None,
            access: None,
            view: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        toml::from_str(&text).map_err(|error| format!("invalid {}: {error}", path.display()))
    }

    /// The URL a client on this machine uses to reach the server.
    pub fn local_url(&self) -> String {
        let addr = self
            .addr
            .unwrap_or_else(|| DEFAULT_ADDR.parse().expect("valid default address"));
        let host = if addr.ip().is_unspecified() {
            "127.0.0.1".to_owned()
        } else if addr.is_ipv6() {
            format!("[{}]", addr.ip())
        } else {
            addr.ip().to_string()
        };
        format!("http://{host}:{}", addr.port())
    }
}

/// Where installers put the configuration on this system.
pub fn default_path() -> PathBuf {
    if cfg!(windows) {
        let data = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(data).join("Grafeno").join("grafeno.toml")
    } else {
        PathBuf::from("/etc/grafeno/grafeno.toml")
    }
}

/// A random token of 32 URL-safe characters (192 bits).
pub fn new_token() -> Result<String, String> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| format!("no random source: {error}"))?;
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[(*b & 63) as usize] as char)
        .collect())
}

/// Writes a new configuration with a fresh token. Refuses to replace an
/// existing file unless `force`.
pub fn init(path: &Path, db: &Path, addr: SocketAddr, force: bool) -> Result<Config, String> {
    let config = Config {
        db: Some(db.to_path_buf()),
        addr: Some(addr),
        ..Config::default()
    };
    write_new(path, config, force)
}

/// Writes `config` (with a fresh token if it has none) as a new settings
/// file, creating its folder and the database's. Refuses to replace an
/// existing file unless `force`.
pub fn write_new(path: &Path, mut config: Config, force: bool) -> Result<Config, String> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists (use --force to replace it)",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    if let Some(parent) = config.db.as_deref().and_then(Path::parent) {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    if config.token.is_none() {
        config.token = Some(new_token()?);
    }
    let text = format!(
        "# Grafeno server settings. Restart the service after changing them.\n\
         # Every /v1 request needs `Authorization: Bearer <token>` (or a key, or a session).\n\
         # `addr = \"0.0.0.0:7810\"` listens on every network interface.\n\n{}",
        toml::to_string(&config).map_err(|error| error.to_string())?
    );
    std::fs::write(path, text)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    restrict(path);
    Ok(config)
}

/// Makes the file (it holds the token) readable by its owner only.
#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640));
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_a_loadable_config_with_a_token() {
        let dir = std::env::temp_dir().join(format!("grafeno-config-{}", grafeno::NodeId::new()));
        let path = dir.join("grafeno.toml");
        let config = init(
            &path,
            &dir.join("data").join("g.redb"),
            "0.0.0.0:7900".parse().unwrap(),
            false,
        )
        .unwrap();
        assert_eq!(config.token.as_ref().unwrap().len(), 32);
        assert_eq!(Config::load(&path).unwrap(), config);
        assert_eq!(config.local_url(), "http://127.0.0.1:7900");
        assert!(dir.join("data").is_dir());
        assert!(
            init(
                &path,
                &dir.join("g.redb"),
                "127.0.0.1:1".parse().unwrap(),
                false
            )
            .is_err()
        );
        assert_ne!(new_token().unwrap(), new_token().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn older_settings_with_studio_still_load() {
        // `studio` served the interface before Grafeno 15; it is ignored now.
        let config: Config = toml::from_str("studio = false").unwrap();
        assert!(!toml::to_string(&config).unwrap().contains("studio"));
    }

    #[test]
    fn rejects_unknown_settings() {
        assert!(toml::from_str::<Config>("port = 1").is_err());
        let config: Config =
            toml::from_str("addr = \"[::1]:7810\"\ncors_origins = [\"*\"]").unwrap();
        assert_eq!(config.local_url(), "http://[::1]:7810");
    }
}
