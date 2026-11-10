//! Running the HTTP server, from the command line or as a system service.
//!
//! On Windows, `grafeno service install` registers a service (started by the
//! Service Control Manager with `grafeno service run --config …`). On Linux,
//! a systemd unit runs `grafeno serve --config /etc/grafeno/grafeno.toml`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use grafeno::Grafeno;

use crate::config::{Config, DEFAULT_ADDR};
use crate::http::{self, AppState, Options};

/// Everything `grafeno serve` needs, once the config file, the environment
/// and the command line have been combined (the command line wins).
#[derive(Clone, Debug)]
pub struct ServerSettings {
    /// `None`: an in-memory database.
    pub db: Option<PathBuf>,
    pub addr: SocketAddr,
    pub token: Option<String>,
    pub options: Options,
    /// Limits for each GQ query.
    pub limits: grafeno_query::Limits,
    /// Threads parallel scans may use at once, over every query (0: one
    /// per CPU; 1: none).
    pub scan_threads: usize,
    pub durability: grafeno::Durability,
    /// Requests served at once.
    pub max_requests: usize,
    /// Cache sizes.
    pub memory: grafeno::OpenOptions,
    /// The group-commit writer, and records per bulk-import transaction.
    pub writer: grafeno::WriterOptions,
    pub import_chunk: usize,
    /// The change log's window.
    pub changes: grafeno::Retention,
    /// Explicit transactions (16.4).
    pub transactions: crate::transactions::TransactionSettings,
    /// How much of the audit log is kept (14.5).
    pub audit: grafeno::AuditRetention,
    /// Work that runs from the change log.
    pub background: crate::events::BackgroundConfig,
    /// Membership of a cluster, if this server is a member.
    pub cluster: Option<crate::cluster::ClusterConfig>,
    /// Sessions and lockouts.
    pub auth: crate::accounts::AuthConfig,
}

/// Values given on the command line (or in the environment), if any.
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub db: Option<PathBuf>,
    pub memory: bool,
    pub addr: Option<SocketAddr>,
    pub token: Option<String>,
    pub no_studio: bool,
    pub cors_origins: Vec<String>,
}

impl ServerSettings {
    pub fn resolve(config: Option<Config>, overrides: Overrides) -> Self {
        let config = config.unwrap_or_default();
        let db = if overrides.memory {
            None
        } else {
            Some(
                overrides
                    .db
                    .or(config.db)
                    .unwrap_or_else(|| PathBuf::from("grafeno.redb")),
            )
        };
        let mut cors_origins = config.cors_origins;
        cors_origins.extend(overrides.cors_origins);
        Self {
            db,
            addr: overrides
                .addr
                .or(config.addr)
                .unwrap_or_else(|| DEFAULT_ADDR.parse().expect("valid default address")),
            token: overrides
                .token
                .or(config.token)
                .filter(|token| !token.is_empty()),
            options: Options { cors_origins },
            limits: config.query.limits(),
            scan_threads: config.query.scan_threads.unwrap_or(0),
            durability: config.durability.map(Into::into).unwrap_or_default(),
            max_requests: config.max_requests.unwrap_or(http::MAX_REQUESTS),
            memory: grafeno::OpenOptions {
                copy_before_upgrade: config.copy_before_upgrade.unwrap_or(true),
                ..config.memory.options()
            },
            writer: config.writer.options(),
            import_chunk: config.writer.import_chunk(),
            changes: config.changes.retention(),
            transactions: config.transactions.settings(),
            audit: config.audit.retention(),
            background: config.background.clone(),
            cluster: config.cluster.clone(),
            auth: config.auth.config(),
        }
    }

    /// What `GET /v1/settings` reports: everything but the token and the
    /// cluster secret (whether there is a token is reported). Sizes in
    /// bytes, durations in seconds or milliseconds as their names say.
    pub fn public_view(&self) -> serde_json::Value {
        use serde_json::json;
        let secs = |d: Option<std::time::Duration>| d.map(|d| d.as_secs_f64());
        let cluster = self.cluster.as_ref().map(|c| {
            let mut value = serde_json::to_value(c).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.remove("secret");
            }
            value
        });
        json!({
            "db": self.db,
            "memory_only": self.db.is_none(),
            "addr": self.addr.to_string(),
            "token": self.token.is_some(),
            "cors_origins": self.options.cors_origins,
            "durability": match self.durability {
                grafeno::Durability::Immediate => "immediate",
                grafeno::Durability::Eventual => "eventual",
            },
            "max_requests": self.max_requests,
            "copy_before_upgrade": self.memory.copy_before_upgrade,
            "query": {
                "timeout_secs": secs(self.limits.timeout),
                "max_reads": self.limits.max_reads,
                "max_rows": self.limits.max_rows,
                "max_memory_bytes": self.limits.max_memory,
                "max_cost": self.limits.max_cost,
                "scan_threads": self.scan_threads,
            },
            "memory": {
                "cache_bytes": self.memory.cache_bytes,
                "projection_cache_bytes": self.memory.projection_cache_bytes,
                "vector_cache_bytes": self.memory.vector_cache_bytes,
            },
            "writer": {
                "max_batch": self.writer.max_batch,
                "queue": self.writer.queue,
                "flush_ms": self.writer.flush_every.as_millis() as u64,
                "import_chunk": self.import_chunk,
            },
            "transactions": {
                "idle_timeout_secs": self.transactions.idle.as_secs_f64(),
                "max_secs": self.transactions.lifetime.as_secs_f64(),
                "wait_secs": self.transactions.wait.as_secs_f64(),
                "max_open": self.transactions.max_open,
            },
            "changes": {
                "enabled": self.changes.enabled,
                "max_entries": self.changes.max_entries,
                "max_age_secs": self.changes.max_age_secs,
            },
            "audit": {
                "max_entries": self.audit.max_entries,
                "max_age_secs": self.audit.max_age_secs,
            },
            "auth": {
                "session_secs": self.auth.session.as_secs(),
                "refresh_secs": self.auth.refresh.as_secs(),
                "lockout_attempts": self.auth.lockout.account_attempts,
                "address_attempts": self.auth.lockout.address_attempts,
                "lockout_secs": self.auth.lockout.window.as_secs(),
            },
            "background": self.background,
            "cluster": cluster,
        })
    }

    fn open(&self) -> grafeno::Result<Grafeno> {
        match &self.db {
            Some(path) => Grafeno::open_with(path, &self.memory),
            None => Grafeno::in_memory(),
        }
    }
}

/// Opens the database and serves until `shutdown` completes.
pub fn run(
    settings: ServerSettings,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = Arc::new(settings.open()?);
    grafeno_query::set_parallelism(settings.scan_threads);
    db.set_durability(settings.durability);
    db.set_change_retention(settings.changes);
    db.set_audit_retention(settings.audit);
    let mut state = AppState::with_writer(db.clone(), settings.token.as_deref(), settings.writer)
        .with_background(settings.background.clone())
        .with_transactions(settings.transactions)
        .with_import_chunk(settings.import_chunk)
        .with_limits(settings.limits.clone())
        .with_max_requests(settings.max_requests)
        .with_auth(settings.auth)
        .with_settings(settings.public_view());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut more_groups: Vec<AppState> = Vec::new();
    if let Some(cluster) = &settings.cluster {
        let node = crate::cluster::start(db, settings.db.as_deref(), cluster)?;
        let apis: std::collections::HashMap<u64, String> = cluster
            .peers
            .iter()
            .filter_map(|peer| peer.api.clone().map(|api| (peer.id, api)))
            .collect();
        let split_cost = cluster
            .split_cost
            .map_or(crate::cluster::DEFAULT_SPLIT_COST, |cost| cost as f64);
        state = state.with_cluster(
            node.clone(),
            apis.clone(),
            split_cost,
            Some(cluster.secret.as_str()),
        );
        // More groups (13.5): the ones this member votes in.
        let mut nodes = std::collections::HashMap::new();
        for group in &cluster.groups {
            if !group.voters.contains(&cluster.id) {
                continue;
            }
            let group_db = Arc::new(match &settings.db {
                Some(path) => Grafeno::open_with(
                    crate::cluster::group_db_path(path, group.id),
                    &settings.memory,
                )?,
                None => Grafeno::in_memory()?,
            });
            group_db.set_durability(settings.durability);
            group_db.set_change_retention(settings.changes);
            let group_path = settings
                .db
                .as_deref()
                .map(|path| crate::cluster::group_db_path(path, group.id));
            let group_node = crate::cluster::start_group(
                group_db.clone(),
                group_path.as_deref(),
                cluster,
                group.id,
                if group.join { &[] } else { &group.voters },
            )?;
            nodes.insert(group.id, group_node.clone());
            let group_apis = apis
                .iter()
                .filter(|(id, _)| group.voters.contains(id))
                .map(|(id, api)| (*id, api.clone()))
                .collect();
            more_groups.push(state.for_group(group.id, group_db, group_node, group_apis));
        }
        runtime.block_on(crate::cluster::listen_groups(node, nodes, cluster))?;
        if !cluster.groups.is_empty() {
            let mut voters = std::collections::BTreeMap::new();
            voters.insert(0, cluster.voters.clone());
            for group in &cluster.groups {
                voters.insert(group.id, group.voters.clone());
            }
            let mut states = std::collections::BTreeMap::new();
            states.insert(0, state.clone());
            for group in &more_groups {
                states.insert(group.group, group.clone());
            }
            let routers = states
                .iter()
                .map(|(id, group)| (*id, http::router_with(group.clone(), &settings.options)))
                .collect();
            let registry = Arc::new(crate::groups::Registry {
                states,
                routers,
                voters,
                apis: apis.into_iter().collect(),
                me: cluster.id,
            });
            let _ = state.groups.set(registry);
        }
    }
    // Each group's webhooks and background work (on its own leader).
    runtime.block_on(async {
        for group in &more_groups {
            crate::events::start(group);
        }
    });
    runtime.block_on(http::serve_until(
        state,
        settings.addr,
        settings.options,
        shutdown,
    ))?;
    Ok(())
}

/// Serves until Ctrl+C (or SIGTERM on Unix).
pub fn run_until_signal(settings: ServerSettings) -> Result<(), Box<dyn std::error::Error>> {
    run(settings, http::shutdown_signal())
}

/// Serves until Ctrl+C, SIGTERM, or standard input closing (the program
/// that started the engine closed it, or ended): `grafeno serve
/// --stop-with-stdin`.
pub fn run_until_signal_or_stdin(
    settings: ServerSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    let (closed, wait) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 256];
        // Whatever is written is ignored; the end of the stream stops it.
        while matches!(stdin.read(&mut buffer), Ok(n) if n > 0) {}
        let _ = closed.send(());
    });
    run(settings, async move {
        tokio::select! {
            _ = http::shutdown_signal() => {}
            _ = wait => {}
        }
    })
}

#[cfg(windows)]
pub mod windows {
    //! The Windows service, named `Grafeno`.

    use std::ffi::OsString;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{Overrides, ServerSettings};
    use crate::config::Config;

    pub const NAME: &str = "Grafeno";
    const DISPLAY_NAME: &str = "Grafeno graph engine";

    static CONFIG: OnceLock<PathBuf> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    /// Called by the Service Control Manager (`grafeno service run`). It
    /// fails when started from a console.
    pub fn dispatch(config: PathBuf) -> Result<(), String> {
        let _ = CONFIG.set(config);
        service_dispatcher::start(NAME, ffi_service_main).map_err(|error| {
            format!("{error} (this command is run by Windows; to serve from a console, use `grafeno serve`)")
        })
    }

    /// The size at which `grafeno.log` becomes `grafeno.log.1` (replacing
    /// the one before).
    const LOG_LIMIT: u64 = 10 * 1024 * 1024;

    /// Services have no console: what happens goes to the Event Log
    /// (Application, source `Grafeno`, which the installer registers) and
    /// to `grafeno.log` next to the configuration, rotated at 10 MiB.
    fn log(config: &Path, message: &str) {
        let error = message.starts_with("error") || message.contains("with an error");
        event(message, error);
        let path = config.with_file_name("grafeno.log");
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{} {message}", grafeno::Timestamp::now().iso());
        }
    }

    /// One event in the Application log (message 1 of grafeno.exe's
    /// message table is the string itself).
    fn event(message: &str, error: bool) {
        use windows_sys::Win32::System::EventLog::{
            DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE,
            RegisterEventSourceW, ReportEventW,
        };
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
        let source = wide(NAME);
        let text = wide(message);
        let strings = [text.as_ptr()];
        // SAFETY: valid NUL-terminated strings that outlive the calls; the
        // handle is released before returning.
        unsafe {
            let handle = RegisterEventSourceW(std::ptr::null(), source.as_ptr());
            if handle.is_null() {
                return;
            }
            let kind = if error {
                EVENTLOG_ERROR_TYPE
            } else {
                EVENTLOG_INFORMATION_TYPE
            };
            ReportEventW(
                handle,
                kind,
                0,
                1,
                std::ptr::null_mut(),
                1,
                0,
                strings.as_ptr(),
                std::ptr::null(),
            );
            DeregisterEventSource(handle);
        }
    }

    fn service_main(_arguments: Vec<OsString>) {
        let config = CONFIG
            .get()
            .cloned()
            .unwrap_or_else(crate::config::default_path);
        if let Err(error) = run_service(&config) {
            log(&config, &format!("error: {error}"));
        }
    }

    fn run_service(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let stop = Mutex::new(Some(stop));
        let status = service_control_handler::register(NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(stop) = stop.lock().ok().and_then(|mut stop| stop.take()) {
                    let _ = stop.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;
        let report = |state: ServiceState, exit: u32| {
            status.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: if state == ServiceState::Running {
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
                } else {
                    ServiceControlAccept::empty()
                },
                exit_code: ServiceExitCode::Win32(exit),
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            })
        };

        let settings = Config::load(config_path)
            .map(|config| ServerSettings::resolve(Some(config), Overrides::default()));
        let result = match settings {
            Ok(settings) => {
                report(ServiceState::Running, 0)?;
                log(config_path, &format!("serving on http://{}", settings.addr));
                super::run(settings, async {
                    let _ = stopped.await;
                })
            }
            Err(error) => Err(error.into()),
        };
        if let Err(error) = &result {
            log(config_path, &format!("stopped with an error: {error}"));
        } else {
            log(config_path, "stopped");
        }
        report(ServiceState::Stopped, u32::from(result.is_err()))?;
        Ok(())
    }

    /// Registers the service (started automatically with Windows), and
    /// starts it unless `start` is false. It runs as `account` (such as
    /// `NT SERVICE\Grafeno`, a virtual account with no rights but those
    /// given to it), or as LocalSystem. Needs an administrator.
    pub fn install(
        config: &Path,
        start: bool,
        account: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;
        let info = ServiceInfo {
            name: NAME.into(),
            display_name: DISPLAY_NAME.into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments: vec![
                "service".into(),
                "run".into(),
                "--config".into(),
                config.into(),
            ],
            dependencies: vec![],
            account_name: account.map(OsString::from),
            account_password: None,
        };
        let service =
            manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
        service.set_description(
            "Grafeno graph database engine (HTTP API on 127.0.0.1:7810 by default).",
        )?;
        if start {
            service.start::<&str>(&[])?;
        }
        Ok(())
    }

    /// Stops the service, waiting up to 15 seconds.
    pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service =
            manager.open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            let _ = service.stop();
            for _ in 0..30 {
                if service.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Ok(())
    }

    /// Whether the service is registered, running and started with Windows.
    pub fn state() -> Option<crate::instances::ServiceState> {
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
        let service = manager
            .open_service(
                NAME,
                ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
            )
            .ok()?;
        let state = service.query_status().ok()?.current_state;
        let enabled = service
            .query_config()
            .is_ok_and(|config| config.start_type == ServiceStartType::AutoStart);
        Some(crate::instances::ServiceState {
            unit: NAME.to_owned(),
            active: state == ServiceState::Running,
            enabled,
            detail: format!("{state:?}").to_lowercase(),
        })
    }

    /// Stops (waiting up to 15 seconds) and removes the service.
    pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(
            NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )?;
        if service.query_status()?.current_state != ServiceState::Stopped {
            let _ = service.stop();
            for _ in 0..30 {
                if service.query_status()?.current_state == ServiceState::Stopped {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        service.delete()?;
        Ok(())
    }
}
