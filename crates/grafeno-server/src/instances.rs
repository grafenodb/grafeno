//! Instances of the engine on one machine (phase 15.2), each a database
//! server of its own: `grafeno setup` initializes one, `grafeno instances`
//! lists, drops and rolls them back, and `grafeno doctor` checks them.
//!
//! On Linux an instance is a settings file in `/etc/grafeno` and a systemd
//! unit: `default` is `grafeno.toml`, served by `grafeno.service` with its
//! data in `/var/lib/grafeno`; any other name is `<name>.toml`, served by
//! the templated `grafeno@<name>.service` with its data in
//! `/var/lib/grafeno/<name>`. On Windows there is one instance, the
//! `Grafeno` service with `C:\ProgramData\Grafeno\grafeno.toml`.
//!
//! `GRAFENO_CONFIG_DIR` and `GRAFENO_DATA_DIR` move the two folders (for
//! tests and unusual layouts).

use std::io::{BufRead, IsTerminal, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;

use grafeno::Grafeno;
use serde::Deserialize;

use crate::config::{self, Config, DurabilitySetting, MemoryConfig};

/// The instance a bare `grafeno setup` creates.
pub const DEFAULT: &str = "default";
/// The port of the default instance; others take the next free one.
pub const DEFAULT_PORT: u16 = 7810;

/// One instance: its name and settings file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    pub config: PathBuf,
}

impl Instance {
    pub fn named(name: &str) -> Result<Self, String> {
        check_name(name)?;
        let file = if name == DEFAULT {
            "grafeno.toml".to_owned()
        } else {
            format!("{name}.toml")
        };
        Ok(Self {
            name: name.to_owned(),
            config: config_dir().join(file),
        })
    }

    /// The systemd unit that serves it.
    pub fn unit(&self) -> String {
        if self.name == DEFAULT {
            "grafeno.service".to_owned()
        } else {
            format!("grafeno@{}.service", self.name)
        }
    }

    /// Where its data goes unless `grafeno setup --data` says otherwise.
    pub fn default_data(&self) -> PathBuf {
        if self.name == DEFAULT {
            data_dir()
        } else {
            data_dir().join(&self.name)
        }
    }
}

/// Names are short, lowercase, and safe in file and unit names.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name.as_bytes()[0].is_ascii_alphanumeric();
    if !ok {
        return Err(format!(
            "{name:?} is not an instance name: up to 32 lowercase letters, digits, - and _"
        ));
    }
    if name == "grafeno" {
        return Err("\"grafeno\" is the default instance's file; call it \"default\"".into());
    }
    if cfg!(windows) && name != DEFAULT {
        return Err("on Windows there is one instance, the Grafeno service (\"default\")".into());
    }
    Ok(())
}

/// The folder of the settings files.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GRAFENO_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    config::default_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The folder the instances' data goes in by default.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("GRAFENO_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        config_dir().join("data")
    } else {
        PathBuf::from("/var/lib/grafeno")
    }
}

/// Every instance with a settings file, the default first.
pub fn list() -> Vec<Instance> {
    let mut found: Vec<Instance> = std::fs::read_dir(config_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?.to_owned();
            let name = if stem == "grafeno" {
                DEFAULT.to_owned()
            } else {
                check_name(&stem).ok()?;
                stem
            };
            Some(Instance { name, config: path })
        })
        .collect();
    found.sort_by(|a, b| (a.name != DEFAULT, &a.name).cmp(&(b.name != DEFAULT, &b.name)));
    found
}

// ---------------------------------------------------------------- setup

/// Everything `grafeno setup` asks, from flags, an answers file or the
/// prompts. Missing answers take the defaults.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answers {
    pub instance: Option<String>,
    /// The folder of the database.
    pub data: Option<PathBuf>,
    /// The whole address (wins over `port` and `network`).
    pub addr: Option<SocketAddr>,
    pub port: Option<u16>,
    /// Listen on every interface instead of this machine only.
    pub network: Option<bool>,
    pub durability: Option<DurabilitySetting>,
    /// Memory for caches, in MiB (half for pages, a quarter each for graphs
    /// and vectors).
    pub memory_mb: Option<u64>,
    /// The first administrator (a superuser who signs in with a password).
    pub admin: Option<String>,
    /// A file whose first line is the administrator's password.
    pub admin_password_file: Option<PathBuf>,
    /// No administrator: only the token works until one is created.
    #[serde(default)]
    pub no_admin: bool,
    /// Enable and start the service (true by default).
    pub start: Option<bool>,
}

impl Answers {
    /// The answers of a TOML file, under these flags (the flags win).
    pub fn merged_over(self, file: Option<&Path>) -> Result<Answers, String> {
        let Some(file) = file else {
            return Ok(self);
        };
        let text = std::fs::read_to_string(file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        let base: Answers = toml::from_str(&text)
            .map_err(|error| format!("invalid {}: {error}", file.display()))?;
        Ok(Answers {
            instance: self.instance.or(base.instance),
            data: self.data.or(base.data),
            addr: self.addr.or(base.addr),
            port: self.port.or(base.port),
            network: self.network.or(base.network),
            durability: self.durability.or(base.durability),
            memory_mb: self.memory_mb.or(base.memory_mb),
            admin: self.admin.or(base.admin),
            admin_password_file: self.admin_password_file.or(base.admin_password_file),
            no_admin: self.no_admin || base.no_admin,
            start: self.start.or(base.start),
        })
    }
}

/// How `grafeno setup` runs.
pub struct SetupMode {
    /// Ask what the answers leave open (otherwise take the defaults).
    pub interactive: bool,
    /// Read the administrator's password from standard input.
    pub password_stdin: bool,
    /// Print only errors (the packages' scripts).
    pub quiet: bool,
}

/// Initializes an instance: its data folder, its settings file (a fresh
/// token), its first administrator, and its service, enabled and started.
pub fn setup(answers: Answers, mode: &SetupMode) -> Result<Instance, String> {
    let interactive = mode.interactive && std::io::stdin().is_terminal();
    let mut prompt = Prompt { interactive };

    let name = match answers.instance.clone() {
        Some(name) => name,
        None => prompt.ask("Instance name", DEFAULT)?,
    };
    let instance = Instance::named(&name)?;
    if instance.config.exists() {
        return Err(format!(
            "the instance {name:?} exists already ({}); `grafeno instances drop {name}` removes it",
            instance.config.display()
        ));
    }

    let data = match answers.data.clone() {
        Some(data) => data,
        None => PathBuf::from(prompt.ask(
            "Data folder",
            &instance.default_data().display().to_string(),
        )?),
    };
    let data = absolute(&data);
    let db = data.join("grafeno.redb");
    // A database already there (an instance dropped without --purge, a
    // package removed and installed again) is used as it is.
    let existing = db.exists();

    let addr = match answers.addr {
        Some(addr) => addr,
        None => {
            let suggested = answers.port.unwrap_or_else(|| free_port(&instance));
            let port = if answers.port.is_some() {
                suggested
            } else {
                prompt
                    .ask("Port", &suggested.to_string())?
                    .parse::<u16>()
                    .map_err(|_| "the port is a number from 1 to 65535".to_owned())?
            };
            let network = match answers.network {
                Some(network) => network,
                None => prompt.yes("Accept connections from other computers", false)?,
            };
            let host = if network { "0.0.0.0" } else { "127.0.0.1" };
            format!("{host}:{port}")
                .parse()
                .map_err(|error| format!("{error}"))?
        }
    };
    if let Some(other) = port_taken(&instance, addr.port()) {
        return Err(format!(
            "port {} is used by the instance {other:?}",
            addr.port()
        ));
    }

    let admin = if answers.no_admin {
        None
    } else {
        let name = match answers.admin.clone() {
            Some(name) => Some(name),
            None if interactive => Some(prompt.ask("First administrator's name", "admin")?),
            None if answers.admin_password_file.is_some() || mode.password_stdin => {
                Some("admin".to_owned())
            }
            None => None,
        };
        match name {
            Some(name) => {
                let password = match &answers.admin_password_file {
                    Some(file) => {
                        let text = std::fs::read_to_string(file)
                            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
                        let password = text.lines().next().unwrap_or("").to_owned();
                        crate::auth::check_password(&name, &password)?;
                        password
                    }
                    None => crate::users::read_password(&name, mode.password_stdin)?,
                };
                Some((name, password))
            }
            None => {
                return Err("name the first administrator (--admin, with \
                     --admin-password-file or --admin-password-stdin), or pass --no-admin \
                     (the token alone works until one is created)"
                    .into());
            }
        }
    };
    let start = answers.start.unwrap_or(true);

    // The data folder and the administrator, then the settings (the last,
    // so that a failure leaves no instance behind).
    make_private_dir(&data)?;
    let db_file = Grafeno::open(&db).map_err(|error| error.to_string())?;
    if let Some((name, password)) = &admin {
        let account = grafeno::Account {
            name: name.clone(),
            kind: grafeno::AccountKind::User,
            password_hash: Some(crate::auth::hash_password(password)),
            grants: Vec::new(),
            roles: Vec::new(),
            superuser: true,
            own_only: false,
            disabled: false,
            expires_at: None,
            attributes: Default::default(),
            description: Some("the first administrator, made by grafeno setup".into()),
            created_at: grafeno::Timestamp::now(),
        };
        db_file
            .write(|tx| tx.create_account(&account))
            .map_err(|error| format!("cannot create the administrator {name:?}: {error}"))?;
    }
    drop(db_file);
    let config = Config {
        db: Some(db.clone()),
        addr: Some(addr),
        durability: answers.durability,
        memory: answers
            .memory_mb
            .map(|mb| MemoryConfig {
                cache_mb: Some(mb / 2),
                projection_cache_mb: Some(mb / 4),
                vector_cache_mb: Some(mb / 4),
            })
            .unwrap_or_default(),
        ..Config::default()
    };
    config::write_new(&instance.config, config, false)?;
    system::own(&instance, &data)?;
    let service = system::enable(&instance, &data, addr, start)?;

    if !mode.quiet {
        println!("Grafeno instance {:?} is ready.", instance.name);
        println!("  address   http://{addr}");
        println!(
            "  data      {}{}",
            data.display(),
            if existing {
                " (the database already there)"
            } else {
                ""
            }
        );
        println!("  settings  {}", instance.config.display());
        match &admin {
            Some((name, _)) => println!("  admin     {name} (sign in with its password)"),
            None => println!("  admin     none: use the token (grafeno config token)"),
        }
        println!("  service   {service}");
        println!(
            "Connect with: grafeno shell --config {}",
            instance.config.display()
        );
    }
    Ok(instance)
}

struct Prompt {
    interactive: bool,
}

impl Prompt {
    fn ask(&mut self, question: &str, default: &str) -> Result<String, String> {
        if !self.interactive {
            return Ok(default.to_owned());
        }
        print!("{question} [{default}]: ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let line = line.trim();
        Ok(if line.is_empty() { default } else { line }.to_owned())
    }

    fn yes(&mut self, question: &str, default: bool) -> Result<bool, String> {
        let answer = self.ask(question, if default { "yes" } else { "no" })?;
        Ok(matches!(
            answer.to_ascii_lowercase().as_str(),
            "y" | "yes" | "s" | "si" | "sí"
        ))
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn make_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o750))
            .map_err(|error| format!("cannot restrict {}: {error}", dir.display()))?;
    }
    #[cfg(windows)]
    crate::config::windows_acl::restrict(dir, true)?;
    Ok(())
}

/// The ports other instances' settings use.
fn ports_in_use(except: &Instance) -> Vec<(String, u16)> {
    list()
        .into_iter()
        .filter(|other| other.name != except.name)
        .filter_map(|other| {
            let config = Config::load(&other.config).ok()?;
            let port = config.addr.map_or(DEFAULT_PORT, |addr| addr.port());
            Some((other.name, port))
        })
        .collect()
}

fn port_taken(instance: &Instance, port: u16) -> Option<String> {
    ports_in_use(instance)
        .into_iter()
        .find(|(_, used)| *used == port)
        .map(|(name, _)| name)
}

/// The default port if free, else the next one no instance uses and
/// nothing listens on.
fn free_port(instance: &Instance) -> u16 {
    let used: Vec<u16> = ports_in_use(instance).into_iter().map(|(_, p)| p).collect();
    (DEFAULT_PORT..DEFAULT_PORT + 100)
        .find(|port| !used.contains(port) && TcpListener::bind(("127.0.0.1", *port)).is_ok())
        .unwrap_or(DEFAULT_PORT)
}

// ---------------------------------------------------------- drop, rollback

/// Stops and removes an instance: its service and settings, and with
/// `purge` its database (and the copies and files beside it).
pub fn drop_instance(instance: &Instance, purge: bool) -> Result<(), String> {
    if !instance.config.exists() {
        return Err(format!("there is no instance {:?}", instance.name));
    }
    let config = Config::load(&instance.config)?;
    system::disable(instance)?;
    std::fs::remove_file(&instance.config)
        .map_err(|error| format!("cannot remove {}: {error}", instance.config.display()))?;
    println!(
        "Removed the instance {:?} (its service and settings).",
        instance.name
    );
    match (purge, config.db) {
        (true, Some(db)) => {
            let removed = remove_database(&db)?;
            println!(
                "Removed its data: {removed} files from {}.",
                db.parent().unwrap_or(&db).display()
            );
        }
        (false, Some(db)) => println!(
            "Kept its data in {} (`--purge` removes it).",
            db.parent().unwrap_or(&db).display()
        ),
        (_, None) => {}
    }
    Ok(())
}

/// The database file and the files named after it (its copies before
/// upgrades, a cluster member's Raft state and groups); then its folder if
/// nothing else is left in it.
fn remove_database(db: &Path) -> Result<usize, String> {
    let stem = db
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("grafeno")
        .to_owned();
    let dir = db.parent().unwrap_or(Path::new("."));
    let mut removed = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = name == format!("{stem}.redb")
            || name.starts_with(&format!("{stem}.redb."))
            || name.starts_with(&format!("{stem}.layout-"))
            || name.starts_with(&format!("{stem}.group-"));
        if ours && entry.path().is_file() {
            std::fs::remove_file(entry.path())
                .map_err(|error| format!("cannot remove {}: {error}", entry.path().display()))?;
            removed += 1;
        }
    }
    let _ = std::fs::remove_dir(dir);
    Ok(removed)
}

/// Puts back the copy an upgrade kept, so that the previous version of
/// Grafeno, installed next, opens the data as it was before the upgrade
/// (what was written since is set aside, not deleted). The service is
/// left stopped: starting it with this version would upgrade the file
/// again.
pub fn rollback(instance: &Instance) -> Result<(), String> {
    let config = Config::load(&instance.config)?;
    if config.cluster.is_some() {
        return Err(
            "a cluster member is not rolled back alone: restore it from a member \
             or a backup instead"
                .into(),
        );
    }
    let db = config
        .db
        .ok_or_else(|| "the instance keeps its data in memory".to_owned())?;
    let Some(copy) = grafeno::upgrade_copies(&db).pop() else {
        return Err(format!(
            "no copy beside {}: no upgrade changed its layout since it was made",
            db.display()
        ));
    };
    let version = Grafeno::file_layout(&copy)
        .map_err(|error| error.to_string())?
        .unwrap_or(0);
    system::stop(instance)?;
    let current = Grafeno::file_layout(&db)
        .map_err(|error| format!("{error} (is the server still running?)"))?
        .unwrap_or(0);
    let stem = db.file_stem().and_then(|s| s.to_str()).unwrap_or("grafeno");
    let aside = db.with_file_name(format!(
        "{stem}.before-rollback-{}.redb",
        grafeno::Timestamp::now().as_micros() / 1000
    ));
    std::fs::rename(&db, &aside)
        .map_err(|error| format!("cannot move {}: {error}", db.display()))?;
    std::fs::rename(&copy, &db).map_err(|error| format!("cannot restore the copy: {error}"))?;
    println!(
        "{} is back to layout {version} (the file as the upgrade found it); the data of \
         layout {current} is kept as {}.",
        db.display(),
        aside.display()
    );
    println!(
        "The service is stopped. Install the previous version of Grafeno now (it starts the \
         service), for example: dnf downgrade grafeno, or apt install grafeno=<version>."
    );
    Ok(())
}

// ------------------------------------------------------------------- doctor

/// What one check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Note,
    Warn,
    Fail,
}

pub struct Finding {
    pub verdict: Verdict,
    pub what: String,
}

fn finding(verdict: Verdict, what: impl Into<String>) -> Finding {
    Finding {
        verdict,
        what: what.into(),
    }
}

/// Checks an instance: its settings, folders and their permissions, the
/// database's layout, free disk space, the port and the service.
pub fn doctor(instance: &Instance) -> Vec<Finding> {
    let mut found = Vec::new();
    let config = match Config::load(&instance.config) {
        Ok(config) => {
            found.push(finding(
                Verdict::Ok,
                format!("settings {}", instance.config.display()),
            ));
            config
        }
        Err(error) => {
            found.push(finding(Verdict::Fail, error));
            return found;
        }
    };
    if let Some(problem) = system::exposed(&instance.config) {
        found.push(finding(
            Verdict::Warn,
            format!(
                "{} {problem} (it holds the token)",
                instance.config.display()
            ),
        ));
    }
    if config.token.is_none() {
        found.push(finding(
            Verdict::Note,
            "no token: only accounts and keys (or nobody, without accounts) can call the API",
        ));
    }

    let addr = config
        .addr
        .unwrap_or_else(|| config::DEFAULT_ADDR.parse().expect("valid default address"));
    let service = system::service_state(instance);
    let running = health(&config.local_url());

    let Some(db) = config.db.clone() else {
        found.push(finding(Verdict::Note, "the data is kept in memory"));
        return found;
    };
    let dir = db.parent().map(Path::to_path_buf).unwrap_or_default();
    if !dir.is_dir() {
        found.push(finding(
            Verdict::Fail,
            format!("the data folder {} does not exist", dir.display()),
        ));
    } else {
        found.push(finding(
            Verdict::Ok,
            format!("data folder {}", dir.display()),
        ));
        if let Some(problem) = system::not_writable_by_service(&dir) {
            found.push(finding(
                Verdict::Fail,
                format!("{}: {problem}", dir.display()),
            ));
        }
        if let Some(problem) = system::exposed(&dir) {
            found.push(finding(
                Verdict::Warn,
                format!("{} {problem}", dir.display()),
            ));
        }
    }

    // The layout: from the server if it runs (it holds the file), else
    // from the file.
    let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    match &running {
        Some(info) => {
            let theirs = info["version"].as_str().unwrap_or("?");
            let ours = env!("CARGO_PKG_VERSION");
            if theirs == ours {
                found.push(finding(
                    Verdict::Ok,
                    format!("the server answers on http://{addr} (version {theirs})"),
                ));
            } else {
                found.push(finding(
                    Verdict::Warn,
                    format!(
                        "the server on http://{addr} runs version {theirs}, the installed \
                         program is {ours}: restart the service"
                    ),
                ));
            }
        }
        None if db.exists() => match Grafeno::file_layout(&db) {
            Ok(Some(version)) if version == grafeno::SCHEMA_VERSION => found.push(finding(
                Verdict::Ok,
                format!(
                    "database {} (layout {version}, {})",
                    db.display(),
                    bytes(size)
                ),
            )),
            Ok(Some(version)) if version < grafeno::SCHEMA_VERSION => found.push(finding(
                Verdict::Note,
                format!(
                    "database {} has layout {version}: the next start upgrades it to {}, \
                     keeping a copy first",
                    db.display(),
                    grafeno::SCHEMA_VERSION
                ),
            )),
            Ok(Some(version)) => found.push(finding(
                Verdict::Fail,
                format!(
                    "database {} has layout {version}, newer than this Grafeno ({}): install \
                     the version that wrote it",
                    db.display(),
                    grafeno::SCHEMA_VERSION
                ),
            )),
            Ok(None) => found.push(finding(
                Verdict::Ok,
                format!("database {} (empty)", db.display()),
            )),
            Err(error) => found.push(finding(
                Verdict::Fail,
                format!("database {}: {error}", db.display()),
            )),
        },
        None => found.push(finding(
            Verdict::Note,
            format!("no database yet: the first start creates {}", db.display()),
        )),
    }

    // Free space: a copy before an upgrade needs as much as the file.
    if dir.is_dir() {
        match free_space(&dir) {
            Some(free) if free < size.max(1 << 30) => found.push(finding(
                Verdict::Warn,
                format!(
                    "{} free on the data folder's disk: less than {} (the database, or 1 GiB); \
                     the copy kept before an upgrade needs as much as the file",
                    bytes(free),
                    bytes(size.max(1 << 30))
                ),
            )),
            Some(free) => found.push(finding(
                Verdict::Ok,
                format!("{} free on the disk", bytes(free)),
            )),
            None => {}
        }
    }
    for copy in grafeno::upgrade_copies(&db) {
        let size = std::fs::metadata(&copy).map(|m| m.len()).unwrap_or(0);
        found.push(finding(
            Verdict::Note,
            format!(
                "{} ({}) is the file before the last upgrade: `grafeno instances rollback {}` \
                 goes back to it; remove it when you no longer need to",
                copy.display(),
                bytes(size),
                instance.name
            ),
        ));
    }

    // The port and the service.
    match (&service, &running) {
        (Some(state), _) if state.active && state.enabled => found.push(finding(
            Verdict::Ok,
            format!("service {} running, starts with the system", state.unit),
        )),
        (Some(state), _) if state.active => found.push(finding(
            Verdict::Warn,
            format!(
                "service {} runs but does not start with the system",
                state.unit
            ),
        )),
        (Some(state), _) => found.push(finding(
            Verdict::Warn,
            format!("service {} is not running ({})", state.unit, state.detail),
        )),
        (None, _) => {}
    }
    let service_up = service.as_ref().is_some_and(|s| s.active);
    if running.is_none() {
        if service_up {
            found.push(finding(
                Verdict::Fail,
                format!("the service runs but nothing answers on http://{addr}"),
            ));
        } else if TcpListener::bind(addr).is_err() {
            found.push(finding(
                Verdict::Fail,
                format!("port {} is taken by another program", addr.port()),
            ));
        } else {
            found.push(finding(
                Verdict::Ok,
                format!("port {} is free", addr.port()),
            ));
        }
    }
    found
}

/// The server's `/health`, if it answers.
fn health(url: &str) -> Option<serde_json::Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .build()
        .into();
    let mut response = agent.get(format!("{url}/health")).call().ok()?;
    response.body_mut().read_json().ok()
}

fn bytes(n: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let n = n as f64;
    if n >= GIB {
        format!("{:.1} GiB", n / GIB)
    } else {
        format!("{:.1} MiB", n / MIB)
    }
}

/// Bytes free for this user on the disk holding `path`.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills the struct it is given and reads a valid C string.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: a valid, NUL-terminated wide string and a valid out pointer.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(any(unix, windows)))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

/// Prints findings and tells whether any failed.
pub fn print_findings(instance: &Instance, findings: &[Finding]) -> bool {
    println!("{}:", instance.name);
    let mut failed = false;
    for f in findings {
        let mark = match f.verdict {
            Verdict::Ok => "  ok  ",
            Verdict::Note => "  note",
            Verdict::Warn => "  warn",
            Verdict::Fail => {
                failed = true;
                "  FAIL"
            }
        };
        println!("{mark}  {}", f.what);
    }
    failed
}

/// One row of `grafeno instances`.
pub fn describe(instance: &Instance) -> String {
    let (addr, db) = match Config::load(&instance.config) {
        Ok(config) => (
            config
                .addr
                .map_or_else(|| config::DEFAULT_ADDR.to_owned(), |a| a.to_string()),
            config
                .db
                .map_or_else(|| "(memory)".to_owned(), |d| d.display().to_string()),
        ),
        Err(_) => ("?".into(), "(unreadable settings)".into()),
    };
    let state = system::service_state(instance).map_or_else(
        || "-".to_owned(),
        |s| match (s.active, s.enabled) {
            (true, true) => "running".to_owned(),
            (true, false) => "running (not enabled)".to_owned(),
            (false, true) => format!("stopped ({})", s.detail),
            (false, false) => "stopped, disabled".to_owned(),
        },
    );
    format!("{:12} {:22} {:24} {}", instance.name, addr, state, db)
}

// ---------------------------------------------------- the operating system

/// The state of an instance's service.
pub struct ServiceState {
    pub unit: String,
    pub active: bool,
    pub enabled: bool,
    pub detail: String,
}

#[cfg(unix)]
mod system {
    use super::*;

    fn systemd() -> bool {
        Path::new("/run/systemd/system").is_dir()
            && std::env::var_os("GRAFENO_NO_SYSTEMD").is_none()
    }

    fn run(program: &str, args: &[&str]) -> Result<String, String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("{program}: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(format!(
                "{program} {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    /// The uid and gid of the `grafeno` system user, if there is one.
    fn service_user() -> Option<(u32, u32)> {
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        passwd.lines().find_map(|line| {
            let mut fields = line.split(':');
            if fields.next()? != "grafeno" {
                return None;
            }
            let _ = fields.next();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
    }

    /// Hands the data folder to the service's user, and the settings to
    /// root with the service's group (they hold the token).
    pub fn own(instance: &Instance, data: &Path) -> Result<(), String> {
        let Some((uid, gid)) = service_user().filter(|_| is_root()) else {
            return Ok(());
        };
        for entry in walk(data) {
            std::os::unix::fs::lchown(&entry, Some(uid), Some(gid))
                .map_err(|error| format!("cannot hand {} to grafeno: {error}", entry.display()))?;
        }
        std::os::unix::fs::chown(&instance.config, Some(0), Some(gid)).map_err(|error| {
            format!(
                "cannot hand {} to grafeno: {error}",
                instance.config.display()
            )
        })?;
        Ok(())
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![dir.to_path_buf()];
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    /// Lets the service write a data folder outside `/var/lib/grafeno`
    /// (systemd, AppArmor and SELinux each confine it), then enables and
    /// starts it. Returns what was done, for the summary.
    pub fn enable(
        instance: &Instance,
        data: &Path,
        addr: SocketAddr,
        start: bool,
    ) -> Result<String, String> {
        if !systemd() || !is_root() {
            return Ok(format!(
                "not installed (no systemd, or not root); run: grafeno serve --config {}",
                instance.config.display()
            ));
        }
        let unit = instance.unit();
        if !data.starts_with("/var/lib/grafeno") {
            let dropin = PathBuf::from(format!("/etc/systemd/system/{unit}.d"));
            std::fs::create_dir_all(&dropin).map_err(|e| e.to_string())?;
            std::fs::write(
                dropin.join("data.conf"),
                format!(
                    "# Written by grafeno setup: the instance's data folder.\n[Service]\nReadWritePaths={}\n",
                    data.display()
                ),
            )
            .map_err(|e| e.to_string())?;
            allow_folder(data);
        }
        allow_port(addr.port());
        run("systemctl", &["daemon-reload"])?;
        if start {
            run("systemctl", &["enable", "--now", &unit])?;
            Ok(format!(
                "{unit}, enabled and running (journalctl -u {unit})"
            ))
        } else {
            run("systemctl", &["enable", &unit])?;
            Ok(format!("{unit}, enabled (systemctl start {unit})"))
        }
    }

    /// AppArmor's profile and SELinux's labels for a folder of our own.
    fn allow_folder(dir: &Path) {
        let profile = Path::new("/etc/apparmor.d/grafeno");
        if profile.exists() {
            let local = Path::new("/etc/apparmor.d/local/grafeno");
            let _ = std::fs::create_dir_all("/etc/apparmor.d/local");
            let rule = format!(
                "# grafeno setup: a data folder\n\"{0}/\" rw,\n\"{0}/**\" rwk,\n",
                dir.display()
            );
            let current = std::fs::read_to_string(local).unwrap_or_default();
            if !current.contains(&rule) {
                let _ = std::fs::write(local, current + &rule);
            }
            if Path::new("/sys/kernel/security/apparmor").exists() {
                let _ = run("apparmor_parser", &["-r", "/etc/apparmor.d/grafeno"]);
            }
        }
        if run("selinuxenabled", &[]).is_ok() {
            let pattern = format!("{}(/.*)?", dir.display());
            let _ = run(
                "semanage",
                &["fcontext", "-a", "-t", "grafeno_var_lib_t", &pattern],
            );
            let _ = run("restorecon", &["-R", &dir.display().to_string()]);
        }
    }

    fn allow_port(port: u16) {
        if (7810..=7819).contains(&port) || run("selinuxenabled", &[]).is_err() {
            return;
        }
        let port = port.to_string();
        let _ = run(
            "semanage",
            &["port", "-a", "-t", "grafeno_port_t", "-p", "tcp", &port],
        );
    }

    pub fn disable(instance: &Instance) -> Result<(), String> {
        if !systemd() {
            return Ok(());
        }
        let unit = instance.unit();
        let _ = run("systemctl", &["disable", "--now", &unit]);
        let dropin = PathBuf::from(format!("/etc/systemd/system/{unit}.d"));
        if dropin.join("data.conf").exists() {
            let _ = std::fs::remove_file(dropin.join("data.conf"));
            let _ = std::fs::remove_dir(&dropin);
        }
        let _ = run("systemctl", &["daemon-reload"]);
        Ok(())
    }

    pub fn stop(instance: &Instance) -> Result<(), String> {
        if systemd() {
            let _ = run("systemctl", &["stop", &instance.unit()]);
        }
        Ok(())
    }

    pub fn service_state(instance: &Instance) -> Option<ServiceState> {
        if !systemd() {
            return None;
        }
        let unit = instance.unit();
        let active = Command::new("systemctl")
            .args(["is-active", &unit])
            .output()
            .ok()?;
        let detail = String::from_utf8_lossy(&active.stdout).trim().to_owned();
        let enabled = Command::new("systemctl")
            .args(["is-enabled", &unit])
            .output()
            .is_ok_and(|o| o.status.success());
        Some(ServiceState {
            unit,
            active: active.status.success(),
            enabled,
            detail,
        })
    }

    /// Whether others than the owner and its group may read it.
    pub fn exposed(path: &Path) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        (mode & 0o007 != 0).then(|| format!("is open to every user (mode {:o})", mode & 0o777))
    }

    /// Whether the service's user may write the folder.
    pub fn not_writable_by_service(dir: &Path) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        let (uid, _) = service_user()?;
        let meta = std::fs::metadata(dir).ok()?;
        (meta.uid() != uid && meta.mode() & 0o002 == 0).then(|| {
            "not owned by the grafeno user, so the service cannot write it \
             (chown -R grafeno:grafeno it)"
                .to_owned()
        })
    }
}

#[cfg(windows)]
mod system {
    use super::*;

    pub fn own(_instance: &Instance, _data: &Path) -> Result<(), String> {
        Ok(())
    }

    pub fn enable(
        instance: &Instance,
        _data: &Path,
        _addr: SocketAddr,
        start: bool,
    ) -> Result<String, String> {
        crate::service::windows::install(&instance.config, start, None)
            .map_err(|error| format!("cannot install the service: {error}"))?;
        Ok(format!(
            "{}{}",
            crate::service::windows::NAME,
            if start { ", running" } else { ", installed" }
        ))
    }

    pub fn disable(_instance: &Instance) -> Result<(), String> {
        crate::service::windows::uninstall().map_err(|error| error.to_string())
    }

    pub fn stop(_instance: &Instance) -> Result<(), String> {
        crate::service::windows::stop().map_err(|error| error.to_string())
    }

    pub fn service_state(_instance: &Instance) -> Option<ServiceState> {
        crate::service::windows::state()
    }

    pub fn exposed(path: &Path) -> Option<String> {
        let open = crate::config::windows_acl::open_to(path);
        (!open.is_empty()).then(|| {
            format!(
                "is open to {} (remove the access with icacls, or reinstall)",
                open.join(", ")
            )
        })
    }

    pub fn not_writable_by_service(_dir: &Path) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(check_name("default").is_ok());
        assert!(check_name("grafeno").is_err());
        assert!(check_name("").is_err());
        assert!(check_name("Big").is_err());
        assert!(check_name("-x").is_err());
        assert!(check_name(&"a".repeat(33)).is_err());
        if cfg!(unix) {
            assert!(check_name("team_2-b").is_ok());
        }
    }

    #[test]
    fn answers_files_lose_to_flags() {
        let dir = std::env::temp_dir().join(format!("grafeno-answers-{}", grafeno::NodeId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("answers.toml");
        std::fs::write(
            &file,
            "port = 7900\nnetwork = true\nadmin = \"root\"\nno_admin = false\n",
        )
        .unwrap();
        let flags = Answers {
            port: Some(7901),
            ..Answers::default()
        };
        let merged = flags.merged_over(Some(&file)).unwrap();
        assert_eq!(merged.port, Some(7901));
        assert_eq!(merged.network, Some(true));
        assert_eq!(merged.admin.as_deref(), Some("root"));
        std::fs::write(&file, "colour = 1\n").unwrap();
        assert!(Answers::default().merged_over(Some(&file)).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
