//! `grafeno migrate`: copies the database of a Grafeno installed with the
//! setup (the `Grafeno` Windows service, `C:\ProgramData\Grafeno`) into
//! the user's own settings, as the Microsoft Store package uses them
//! (`crate::packaged`).
//!
//! Without `--apply` it only looks and prints the plan. With it, it takes a
//! consistent copy (from the running service over HTTP, or from the file
//! when the service is stopped), checks it as it restores it into a new
//! file, and writes new settings for it. It never stops, changes or
//! removes the service or its files, never replaces a file, and never
//! prints a token.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};

use crate::config::{self, Config};

/// Where the copy comes from and goes to.
pub struct Migration {
    /// The setup's settings file.
    pub from: PathBuf,
    /// The settings file to create.
    pub to: PathBuf,
    /// The owner token of the service (when its settings cannot be read).
    pub token: Option<String>,
    /// The port of the new settings (default: 7810, or the next free one).
    pub port: Option<u16>,
}

/// The setup's settings file.
pub fn setup_settings() -> PathBuf {
    let data = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    PathBuf::from(data).join("Grafeno").join("grafeno.toml")
}

/// What was found, before anything is done.
pub struct Survey {
    pub service: Option<crate::instances::ServiceState>,
    /// The setup's settings, when this account may read them.
    pub source: Result<Config, String>,
    /// The URL the service answers on, and what it says it is.
    pub answering: Option<(String, serde_json::Value)>,
    pub target_exists: bool,
    pub port: u16,
    pub plan: Vec<String>,
    /// Why it cannot go on, if it cannot.
    pub blocked: Option<String>,
}

fn service_state() -> Option<crate::instances::ServiceState> {
    #[cfg(windows)]
    {
        crate::service::windows::state()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn about(url: &str) -> Option<serde_json::Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(3)))
        .build()
        .into();
    let mut response = agent.get(format!("{url}/v1/about")).call().ok()?;
    response.body_mut().read_json().ok()
}

fn free(port: u16) -> bool {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).is_ok()
}

/// Looks without changing anything.
pub fn survey(migration: &Migration) -> Survey {
    let service = service_state();
    let source = if migration.from.exists() {
        Config::load(&migration.from).map_err(|error| {
            format!(
                "cannot read {} ({error}): it is for SYSTEM and the administrators",
                migration.from.display()
            )
        })
    } else {
        Err(format!(
            "no {} (no setup installation here)",
            migration.from.display()
        ))
    };
    let url = source
        .as_ref()
        .map(|config| config.local_url())
        .unwrap_or_else(|_| format!("http://127.0.0.1:{}", crate::instances::DEFAULT_PORT));
    let answering = about(&url).map(|about| (url.clone(), about));
    let target_exists = migration.to.exists();
    let port = migration.port.unwrap_or_else(|| {
        (crate::instances::DEFAULT_PORT..crate::instances::DEFAULT_PORT + 20)
            .find(|port| free(*port))
            .unwrap_or(crate::instances::DEFAULT_PORT + 1)
    });

    let mut plan = Vec::new();
    let mut blocked = None;
    match &service {
        Some(state) => plan.push(format!(
            "The Grafeno service (setup) is installed and {}; it is left as it is.",
            if state.active { "running" } else { "stopped" }
        )),
        None => plan.push("No Grafeno service (setup) is installed.".into()),
    }
    match &source {
        Ok(config) => plan.push(format!(
            "Its settings: {}; its database: {}.",
            migration.from.display(),
            config
                .db
                .as_ref()
                .map(|db| db.display().to_string())
                .unwrap_or_else(|| "in memory (nothing to copy)".into())
        )),
        Err(error) => plan.push(format!("Its settings: {error}.")),
    }
    let running = service.as_ref().is_some_and(|s| s.active);
    if let Some((url, about)) = &answering {
        let version = about["version"].as_str().unwrap_or("?");
        let layout = about["layout"].as_u64().unwrap_or(0);
        plan.push(format!(
            "A Grafeno {version} server answers on {url} (file layout {layout}; this program reads up to {}).",
            grafeno::SCHEMA_VERSION
        ));
        if layout > grafeno::SCHEMA_VERSION {
            blocked = Some(format!(
                "that server is newer than this Grafeno ({}): update GrafenoDB first",
                env!("CARGO_PKG_VERSION")
            ));
        }
        let token = migration
            .token
            .clone()
            .or_else(|| source.as_ref().ok().and_then(|c| c.token.clone()));
        if token.is_some() {
            plan.push(
                "The copy: a consistent backup from that server (GET /v1/backup), while it keeps serving."
                    .into(),
            );
        } else if blocked.is_none() {
            blocked = Some(
                "the owner token is needed for a backup from the running service: as \
                 administrator, `grafeno config token` (the setup's grafeno) prints it; pass \
                 it with --token, or set GRAFENO_TOKEN"
                    .into(),
            );
        }
    } else if running {
        blocked = Some("the service is running but does not answer: check it with grafeno doctor as administrator".into());
    } else {
        match &source {
            Ok(config) if config.db.is_some() => plan
                .push("The copy: from the database file directly (the service is stopped).".into()),
            Ok(_) => {
                blocked = Some("the setup's database is in memory: there is nothing to copy".into())
            }
            Err(_) => {
                blocked = Some(
                    "the setup's settings cannot be read and its service does not answer: start \
                     the service (Start-Service Grafeno, as administrator) and pass its token \
                     with --token, or run this once as administrator"
                        .into(),
                )
            }
        }
    }
    if target_exists {
        blocked = Some(format!(
            "{} exists already: this copies into new settings only (nothing is replaced)",
            migration.to.display()
        ));
    } else {
        let db = migration
            .to
            .parent()
            .unwrap_or(Path::new("."))
            .join("data")
            .join("grafeno.redb");
        plan.push(format!(
            "Into: new settings {} (a new token; accounts and keys come with the data) and the database {}, checked as it is restored.",
            migration.to.display(),
            db.display()
        ));
        plan.push(format!(
            "The new server listens on 127.0.0.1:{port}{}.",
            if port == crate::instances::DEFAULT_PORT {
                String::new()
            } else {
                format!(
                    " ({} is taken, by the service while it runs)",
                    crate::instances::DEFAULT_PORT
                )
            }
        ));
    }
    plan.push("Nothing of the setup's installation is stopped, changed or removed.".into());
    Survey {
        service,
        source,
        answering,
        target_exists,
        port,
        plan,
        blocked,
    }
}

/// Does what [`survey`] planned. Returns the new database file.
pub fn apply(migration: &Migration, survey: &Survey) -> Result<PathBuf, String> {
    if let Some(blocked) = &survey.blocked {
        return Err(blocked.clone());
    }
    let home = migration
        .to
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let db = home.join("data").join("grafeno.redb");
    if db.exists() {
        return Err(format!(
            "{} exists already (nothing is replaced)",
            db.display()
        ));
    }
    std::fs::create_dir_all(home.join("data"))
        .map_err(|error| format!("cannot create {}: {error}", home.join("data").display()))?;
    match &survey.answering {
        Some((url, _)) => {
            let token = migration
                .token
                .clone()
                .or_else(|| survey.source.as_ref().ok().and_then(|c| c.token.clone()))
                .ok_or("no token")?;
            let temporary = home.join("data").join(".grafeno-migrate.redb");
            let _ = std::fs::remove_file(&temporary);
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            let mut response = agent
                .get(format!("{url}/v1/backup"))
                .header("authorization", format!("Bearer {token}"))
                .call()
                .map_err(|error| format!("cannot reach {url}: {error}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "the service refused the backup (HTTP {}): is the token its owner token?",
                    response.status().as_u16()
                ));
            }
            let bytes = response
                .body_mut()
                .with_config()
                .limit(u64::MAX)
                .read_to_vec()
                .map_err(|error| format!("the backup did not arrive whole: {error}"))?;
            std::fs::write(&temporary, &bytes)
                .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
            let restored = grafeno::Grafeno::recover_backup(&temporary, &db)
                .map_err(|error| format!("the backup did not check out: {error}"));
            let _ = std::fs::remove_file(&temporary);
            restored?;
        }
        None => {
            let source = survey.source.as_ref().map_err(Clone::clone)?;
            let from = source.db.as_ref().ok_or("nothing to copy")?;
            crate::packaged::copy_database(from, &db)?;
        }
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], survey.port));
    if let Err(error) = config::init(&migration.to, &db, addr, false) {
        // The copy stays; only the settings failed.
        return Err(format!(
            "the database was copied to {}, but the settings could not be written: {error}",
            db.display()
        ));
    }
    Ok(db)
}
