//! The `grafeno` command: serves a Grafeno database over HTTP or MCP, runs
//! it as a service, and manages it from a console.

use clap::{Args, Parser, Subcommand};
use grafeno::Grafeno;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use grafeno_server::config::{self, Config};
use grafeno_server::mcp;
use grafeno_server::service::{self, Overrides, ServerSettings};
use grafeno_server::shell::{Backend, Shell, read_script, stdin_script};

#[derive(Parser)]
#[command(
    name = "grafeno",
    version,
    about = "Graph engine for agent memory and knowledge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the JSON HTTP API.
    Serve {
        /// Settings file (`grafeno.toml`); flags and environment variables
        /// take precedence over it.
        #[arg(long, env = "GRAFENO_CONFIG")]
        config: Option<PathBuf>,
        /// Database file, created if missing. Default: grafeno.redb.
        #[arg(long, env = "GRAFENO_DB")]
        db: Option<PathBuf>,
        /// Keep everything in memory instead of a file (lost on exit).
        #[arg(long, conflicts_with = "db")]
        memory: bool,
        /// Address to listen on. Default: 127.0.0.1:7810.
        #[arg(long, env = "GRAFENO_ADDR")]
        addr: Option<SocketAddr>,
        /// If set, every `/v1` request needs `Authorization: Bearer <token>`.
        #[arg(long, env = "GRAFENO_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Ignored since Grafeno 15 (the engine serves no interface); kept
        /// for older scripts.
        #[arg(hide = true)]
        #[arg(long)]
        no_studio: bool,
        /// Allow browsers on this origin to call the API (repeatable; `*`
        /// for any), such as a Studio hosted elsewhere.
        #[arg(long = "cors-origin")]
        cors_origins: Vec<String>,
        /// Stop (cleanly, as on Ctrl+C) when standard input closes: for a
        /// program that starts the engine, such as Grafeno Desktop, so that
        /// it stops with that program even if the program dies.
        #[arg(long)]
        stop_with_stdin: bool,
    },
    /// Initialize an instance: its data folder, settings
    /// (a fresh token), first administrator and service, enabled and
    /// started. Asks what the flags leave open, unless --unattended.
    Setup(SetupArgs),
    /// The instances on this machine; drop one, or roll one back to the
    /// copy its last upgrade kept.
    Instances {
        #[command(subcommand)]
        command: Option<InstancesCommand>,
    },
    /// Check the instances (or one): settings, folders and permissions,
    /// the database's layout, free disk space, the port and the service.
    Doctor {
        /// Only this instance.
        #[arg(long)]
        instance: Option<String>,
    },
    /// Create or read the settings file of an installed server.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Run Grafeno as a Windows service.
    #[command(subcommand)]
    Service(ServiceCommand),
    /// Serve MCP (Model Context Protocol) over stdin and stdout.
    Mcp {
        #[command(flatten)]
        db: DbArgs,
        /// Namespace used by tools that do not name one.
        #[arg(long, env = "GRAFENO_NAMESPACE", default_value = "default")]
        namespace: String,
        /// Namespaces the agent may use, comma-separated (`vault,team-*`);
        /// all by default.
        #[arg(long, env = "GRAFENO_MCP_ALLOW", value_delimiter = ',')]
        allow: Vec<String>,
        /// Offer only the tools that read.
        #[arg(long)]
        read_only: bool,
        /// Record the agent's writes under this name (`@recorded_by`).
        #[arg(long = "as", env = "GRAFENO_MCP_AS")]
        actor: Option<String>,
        /// With --as: the agent may change only what it recorded.
        #[arg(long, requires = "actor")]
        own_only: bool,
        /// Also serve the HTTP API on this address (such as
        /// 127.0.0.1:7810), on the same database: Grafeno Studio and
        /// `grafeno shell --url` connect to it and see what the agent
        /// writes as it writes it. Only one process can open a database
        /// file; this one shares it.
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// With --listen: every /v1 request needs this bearer token (or an
        /// account's key or session).
        #[arg(long, env = "GRAFENO_TOKEN", requires = "listen")]
        token: Option<String>,
        /// With --listen: origins allowed to call the API from a browser.
        #[arg(long, value_delimiter = ',', requires = "listen")]
        cors_origins: Vec<String>,
    },
    /// An interactive console for GQ queries and administration.
    Shell {
        /// Connect to the server of this settings file (its address and token).
        #[arg(long, conflicts_with_all = ["url", "db", "memory"])]
        config: Option<PathBuf>,
        /// Database file to open directly (stop any server using it first).
        #[arg(long, conflicts_with_all = ["url", "memory"])]
        db: Option<PathBuf>,
        /// Use a throwaway in-memory database.
        #[arg(long, conflicts_with = "url")]
        memory: bool,
        /// URL of a running `grafeno serve`, such as http://127.0.0.1:7810;
        /// several members of a cluster separated by commas, tried in turn.
        #[arg(long, env = "GRAFENO_URL")]
        url: Option<String>,
        /// Bearer token of the server.
        #[arg(long, env = "GRAFENO_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Sign in as this user (the password is asked at a prompt).
        #[arg(long, env = "GRAFENO_USER", conflicts_with_all = ["db", "memory"])]
        user: Option<String>,
        /// With --user: read the password from standard input.
        #[arg(long, requires = "user")]
        password_stdin: bool,
        #[arg(
            long,
            short = 'n',
            env = "GRAFENO_NAMESPACE",
            default_value = "default"
        )]
        namespace: String,
        /// Run these statements (and meta-commands) and exit.
        #[arg(long, short = 'c', conflicts_with = "file")]
        command: Option<String>,
        /// Run the statements in this file (`-` for standard input) and exit.
        #[arg(long, short = 'f')]
        file: Option<String>,
        /// Print results as JSON instead of tables.
        #[arg(long)]
        json: bool,
        /// With -c or -f: stop at the first statement that fails (by default
        /// the rest still runs, and the exit code says something failed).
        #[arg(long)]
        stop_on_error: bool,
    },
    /// API keys: names with grants per namespace (read, write, admin).
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Accounts: users who sign in with a password, and service accounts.
    #[command(subcommand)]
    Users(UsersCommand),
    /// Roles: privileges, levels and other roles, given to accounts.
    #[command(subcommand)]
    Roles(RolesCommand),
    /// Grant, deny and revoke privileges on roles, and roles to accounts.
    #[command(subcommand)]
    Grants(GrantsCommand),
    /// A cluster member's view, its voters, and compacting its log (on a
    /// running member, through its API).
    #[command(subcommand)]
    Cluster(ClusterCommand),
    /// Write a consistent copy of the whole database to a new file, from a
    /// file directly or from a running server, which keeps serving.
    Backup {
        /// The new file (it must not exist).
        output: PathBuf,
        #[command(flatten)]
        target: KeysTarget,
    },
    /// Recover a backup into a new database file: its pages checked, its
    /// layout upgraded and its namespaces verified on a private copy; the
    /// backup is only read and an existing file is never replaced.
    Restore {
        /// The backup (`grafeno backup` or `GET /v1/backup`).
        backup: PathBuf,
        /// The new database file (it must not exist).
        output: PathBuf,
    },
    /// List the namespaces with their node and edge counts, or create,
    /// describe or drop one (on a database file; on a server, use GQ:
    /// `CREATE NAMESPACE`, `ALTER NAMESPACE`, `DROP NAMESPACE`).
    Namespaces {
        #[command(flatten)]
        db: FileArgs,
        #[command(subcommand)]
        action: Option<NamespacesCommand>,
    },
    /// Print statistics as JSON (of every namespace, or of the given ones).
    Stats {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n')]
        namespaces: Vec<String>,
    },
    /// Check the file (every page against its checksum), then that indexes
    /// and counters agree with the records. Fails if not.
    Check {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n')]
        namespaces: Vec<String>,
        /// Also estimate the recall@10 of vector searches, from this many of
        /// each namespace's vectors used as queries (reads every vector).
        #[arg(long, value_name = "SAMPLE")]
        recall: Option<usize>,
    },
    /// Build the vector graph of namespaces again from their vectors:
    /// reconnects vectors searches cannot reach and drops dangling links.
    Compact {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n', required_unless_present = "all")]
        namespaces: Vec<String>,
        /// Compact every namespace.
        #[arg(long, conflicts_with = "namespaces")]
        all: bool,
    },
    /// Read new and changed texts and link what they mention (names,
    /// emails, URLs, handles, hashtags, dates, amounts, new names).
    Extract {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n', required_unless_present = "all")]
        namespaces: Vec<String>,
        /// Every namespace.
        #[arg(long, conflicts_with = "namespaces")]
        all: bool,
        /// Read every text again, changed or not.
        #[arg(long)]
        force: bool,
    },
    /// Rebuild the indexes and counters of namespaces from their records.
    Rebuild {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n', required_unless_present = "all")]
        namespaces: Vec<String>,
        /// Rebuild every namespace.
        #[arg(long, conflicts_with = "namespaces")]
        all: bool,
    },
    /// Write namespaces (all by default) as a JSON Lines export, or one
    /// namespace as CSV (`nodes.csv` and `edges.csv` in a folder) or GraphML.
    Export {
        #[command(flatten)]
        db: FileArgs,
        #[arg(long = "namespace", short = 'n')]
        namespaces: Vec<String>,
        /// Output file (a folder for CSV); standard output when absent.
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "jsonl")]
        format: Format,
    },
    /// Import a JSON Lines export in one transaction, or CSV or GraphML
    /// into one namespace.
    Import {
        #[command(flatten)]
        db: FileArgs,
        /// Export file, or `-` for standard input (CSV: the nodes file).
        input: PathBuf,
        /// Load in chunks of 20,000 records with a single flush at the end:
        /// faster for large exports, but an error leaves the chunks before
        /// it in place.
        #[arg(long)]
        bulk: bool,
        #[arg(long, value_enum, default_value = "jsonl")]
        format: Format,
        /// The namespace that CSV and GraphML go into.
        #[arg(long = "namespace", short = 'n')]
        namespace: Option<String>,
        /// CSV: the edges file, read after the nodes.
        #[arg(long)]
        edges: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    Jsonl,
    Csv,
    Graphml,
}

/// Where `grafeno keys` works: a database file (with no server using it),
/// or the running server of a settings file.
#[derive(Args)]
struct KeysTarget {
    /// The server of this settings file (its address and owner token).
    #[arg(long, env = "GRAFENO_CONFIG", conflicts_with = "db")]
    config: Option<PathBuf>,
    /// A database file, opened directly.
    #[arg(long, env = "GRAFENO_DB", default_value = "grafeno.redb")]
    db: PathBuf,
}

#[derive(Args)]
struct MemberTarget {
    /// The member's API address.
    #[arg(long, env = "GRAFENO_URL", default_value = "http://127.0.0.1:7810")]
    url: String,
    /// An owner token or key.
    #[arg(long, env = "GRAFENO_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// The Raft group (0, the default, is the cluster's first).
    #[arg(long, default_value_t = 0)]
    group: u64,
}

#[derive(Subcommand)]
enum ClusterCommand {
    /// Role, term, leader, positions and voters, as this member sees them.
    Status {
        #[command(flatten)]
        target: MemberTarget,
    },
    /// Changes the voting members (ask the leader): the old and new sets
    /// decide together, then the new set alone.
    Voters {
        #[command(flatten)]
        target: MemberTarget,
        /// The new voters' ids.
        #[arg(required = true)]
        ids: Vec<u64>,
    },
    /// Snapshots the member's database and drops its log up to it.
    Compact {
        #[command(flatten)]
        target: MemberTarget,
    },
}

fn cluster_call(
    target: &MemberTarget,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let base = target.url.trim_end_matches('/');
    let url = match (target.group, path) {
        (0, _) => format!("{base}/v1/cluster{path}"),
        (group, "") => format!("{base}/v1/cluster/groups/{group}/status"),
        (group, path) => format!("{base}/v1/cluster/groups/{group}{path}"),
    };
    let auth = target.token.as_ref().map(|token| format!("Bearer {token}"));
    let response = match (method, body) {
        ("GET", _) => {
            let mut request = agent.get(&url);
            if let Some(auth) = &auth {
                request = request.header("Authorization", auth);
            }
            request.call()
        }
        (_, body) => {
            let mut request = agent.post(&url);
            if let Some(auth) = &auth {
                request = request.header("Authorization", auth);
            }
            request.send_json(body.unwrap_or(serde_json::json!({})))
        }
    };
    let mut response = response.map_err(|error| format!("{url}: {error}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    let pretty = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or(text);
    if status.is_success() {
        println!("{pretty}");
        Ok(())
    } else {
        Err(format!("{status}: {pretty}"))
    }
}

#[derive(Subcommand)]
enum KeysCommand {
    /// List the keys and their grants (never their secrets).
    List {
        #[command(flatten)]
        target: KeysTarget,
    },
    /// Create a key and print its secret, once.
    Create {
        #[command(flatten)]
        target: KeysTarget,
        /// A name for it: it is recorded on what it writes.
        name: String,
        /// `namespace:role`, repeatable: `vault:write`, `team-*:read`, `*:admin`.
        #[arg(long = "grant", short = 'g')]
        grants: Vec<String>,
        /// Every right on every namespace, and managing keys.
        #[arg(long)]
        owner: bool,
        /// It may change only nodes and facts it recorded itself.
        #[arg(long)]
        own_only: bool,
        #[arg(long)]
        description: Option<String>,
    },
    /// Revoke a key: its secret stops working at once.
    Revoke {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
}

#[derive(Subcommand)]
enum UsersCommand {
    /// List the accounts and what they may do.
    List {
        #[command(flatten)]
        target: KeysTarget,
    },
    /// Create a user (asks for a password) or a service account.
    Create {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
        /// Every right on every namespace, and managing accounts.
        #[arg(long)]
        superuser: bool,
        /// `namespace:role`, repeatable: `vault:write`, `team-*:read`.
        #[arg(long = "grant", short = 'g')]
        grants: Vec<String>,
        /// A service account: no password, API keys only.
        #[arg(long)]
        service: bool,
        /// It may change only nodes and facts it recorded itself.
        #[arg(long)]
        own_only: bool,
        /// Read the password from standard input instead of a prompt.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Set a user's password (its sessions end).
    Password {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Disable an account: it can neither sign in nor use its keys.
    Disable {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
    /// Enable a disabled account.
    Enable {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
    /// Delete an account with its keys and sessions.
    Delete {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
}

#[derive(Subcommand)]
enum RolesCommand {
    /// List the roles, built-in ones first.
    List {
        #[command(flatten)]
        target: KeysTarget,
    },
    /// Create a role (empty; grant it privileges with `grafeno grants`).
    Create {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a role: every account and role that had it loses it.
    Delete {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
    /// The privileges of a role or an account, with the role each comes from.
    Show {
        #[command(flatten)]
        target: KeysTarget,
        name: String,
    },
}

#[derive(Subcommand)]
enum GrantsCommand {
    /// Every privilege, by what it applies to.
    Catalog {
        #[command(flatten)]
        target: KeysTarget,
    },
    /// Grant privileges (`read,create`, or a level: reader, editor, admin).
    Grant {
        #[command(flatten)]
        target: KeysTarget,
        privileges: String,
        /// server, namespace:NAME, nodes:Label@NAME, edges:label@NAME or
        /// property:Label.key@NAME (NAME may be a pattern: `team-*`).
        #[arg(long)]
        on: String,
        /// The role.
        #[arg(long)]
        to: String,
    },
    /// Deny privileges: a denial wins over any grant.
    Deny {
        #[command(flatten)]
        target: KeysTarget,
        privileges: String,
        #[arg(long)]
        on: String,
        #[arg(long)]
        to: String,
    },
    /// Revoke privileges (grants and denials; `--grants` or `--denials` only).
    Revoke {
        #[command(flatten)]
        target: KeysTarget,
        privileges: String,
        #[arg(long)]
        on: String,
        #[arg(long)]
        from: String,
        #[arg(long, conflicts_with = "denials")]
        grants: bool,
        #[arg(long)]
        denials: bool,
    },
    /// Give a role to an account or another role.
    AddRole {
        #[command(flatten)]
        target: KeysTarget,
        role: String,
        #[arg(long)]
        to: String,
    },
    /// Take a role from an account or another role.
    RemoveRole {
        #[command(flatten)]
        target: KeysTarget,
        role: String,
        #[arg(long)]
        from: String,
    },
}

impl KeysTarget {
    fn resolve(&self) -> Result<grafeno_server::keys::Target<'_>, String> {
        match &self.config {
            Some(config) => grafeno_server::keys::Target::server(config),
            None => Ok(grafeno_server::keys::Target::File(&self.db)),
        }
    }
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Write a settings file with a new random token.
    Init {
        /// Where to write it. Default: the system location (`grafeno config path`).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Database file of the server. Default: grafeno.redb in a data folder next to the settings.
        #[arg(long)]
        db: Option<PathBuf>,
        #[arg(long, default_value = grafeno_server::config::DEFAULT_ADDR)]
        addr: SocketAddr,
        /// Replace an existing file (and its token).
        #[arg(long)]
        force: bool,
    },
    /// Print where installers keep the settings on this system.
    Path,
    /// Print the token of a settings file (for Grafeno Studio or clients).
    Token {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Args)]
struct SetupArgs {
    /// The instance's name: `default` (grafeno.toml, grafeno.service) or
    /// another (<name>.toml, grafeno@<name>.service).
    #[arg(long)]
    instance: Option<String>,
    /// The folder of its database.
    #[arg(long)]
    data: Option<PathBuf>,
    /// The address to listen on (wins over --port and --network).
    #[arg(long)]
    addr: Option<std::net::SocketAddr>,
    /// The port (the default instance 7810, others the next free one).
    #[arg(long)]
    port: Option<u16>,
    /// Accept connections from other computers (every interface).
    #[arg(long)]
    network: bool,
    /// `immediate` (the default) or `eventual`.
    #[arg(long)]
    durability: Option<String>,
    /// Memory for caches, in MiB.
    #[arg(long)]
    memory_mb: Option<u64>,
    /// The first administrator's name (a superuser).
    #[arg(long)]
    admin: Option<String>,
    /// A file whose first line is the administrator's password.
    #[arg(long)]
    admin_password_file: Option<PathBuf>,
    /// Read the administrator's password from standard input.
    #[arg(long)]
    admin_password_stdin: bool,
    /// No administrator: the token alone works until one is created.
    #[arg(long)]
    no_admin: bool,
    /// Enable the service without starting it.
    #[arg(long)]
    no_start: bool,
    /// Answers in a TOML file (the same names as the flags, with `_`).
    #[arg(long)]
    answers: Option<PathBuf>,
    /// Ask nothing: take the defaults for what is not given.
    #[arg(long, short = 'y')]
    unattended: bool,
    /// Print only errors.
    #[arg(long, short = 'q')]
    quiet: bool,
}

#[derive(Subcommand)]
enum InstancesCommand {
    /// List them (the default).
    List,
    /// Stop and remove an instance (its service and settings); with
    /// --purge its database too.
    Drop {
        name: String,
        /// Remove its database and the copies beside it.
        #[arg(long)]
        purge: bool,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Put back the copy the last upgrade kept, for the previous version
    /// (install it next); the newer data is set aside, not deleted.
    Rollback { name: String },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Register the `Grafeno` service and start it (as administrator).
    Install {
        /// Settings file the service reads. Default: the system location.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Register it without starting it.
        #[arg(long)]
        no_start: bool,
        /// The account it runs as, such as `NT SERVICE\Grafeno` (a virtual
        /// account); LocalSystem by default.
        #[arg(long)]
        account: Option<String>,
    },
    /// Stop and remove the service (as administrator).
    Uninstall,
    /// Entry point used by Windows to run the service.
    #[command(hide = true)]
    Run {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum NamespacesCommand {
    /// Create an empty namespace.
    Create {
        name: String,
        /// What the namespace is for.
        #[arg(long)]
        description: Option<String>,
        /// Succeed if it exists already.
        #[arg(long)]
        if_not_exists: bool,
    },
    /// Set the description of a namespace (none: remove it).
    Describe {
        name: String,
        description: Option<String>,
    },
    /// Delete a namespace and everything in it.
    Drop {
        name: String,
        /// Confirm: everything in the namespace is deleted.
        #[arg(long)]
        yes: bool,
    },
}

/// The database file of an administration command. Stop any server using
/// it first: only one process can open it.
#[derive(Args)]
struct FileArgs {
    #[arg(long, env = "GRAFENO_DB", default_value = "grafeno.redb")]
    db: PathBuf,
}

impl FileArgs {
    fn open(&self) -> grafeno::Result<Grafeno> {
        Grafeno::open(&self.db)
    }
}

/// The given namespaces, or every namespace when none is given.
fn namespaces_or_all(db: &Grafeno, namespaces: Vec<String>) -> grafeno::Result<Vec<String>> {
    if !namespaces.is_empty() {
        return Ok(namespaces);
    }
    Ok(db
        .begin_read()?
        .namespaces()?
        .into_iter()
        .map(|info| info.name)
        .collect())
}

#[derive(Args)]
struct DbArgs {
    /// Database file, created if missing. Only one process can open it at a time.
    #[arg(long, env = "GRAFENO_DB", default_value = "grafeno.redb")]
    db: PathBuf,
    /// Keep everything in memory instead of a file (lost on exit).
    #[arg(long, conflicts_with = "db")]
    memory: bool,
}

impl DbArgs {
    fn open(&self) -> Result<Grafeno, Box<dyn std::error::Error>> {
        if self.memory {
            return Ok(Grafeno::in_memory()?);
        }
        Grafeno::open(&self.db).map_err(|error| held_elsewhere(&self.db, error))
    }
}

/// An error opening a database file, saying what to do when another
/// process holds it.
fn held_elsewhere(path: &std::path::Path, error: grafeno::Error) -> Box<dyn std::error::Error> {
    let text = error.to_string();
    if text.contains("already open") {
        format!(
            "{} is open in another process (a grafeno serve, a grafeno mcp or a console): \
             only one process can open a database file. Stop it, or connect to it instead: \
             start the agent's server with `grafeno mcp --db {} --listen 127.0.0.1:7810` and \
             open http://127.0.0.1:7810 in Grafeno Studio, or `grafeno shell --url \
             http://127.0.0.1:7810`",
            path.display(),
            path.display()
        )
        .into()
    } else {
        error.into()
    }
}

/// Serves the HTTP API on `db`, in threads of its own, while this process
/// serves MCP on its standard input and output. Refused at once if the
/// address is taken.
fn serve_beside_mcp(
    db: std::sync::Arc<Grafeno>,
    addr: SocketAddr,
    token: Option<String>,
    cors_origins: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Bound here, so that a taken address is an error now and not a line
    // in a log nobody reads.
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|error| format!("cannot serve the HTTP API on {addr}: {error}"))?;
    listener.set_nonblocking(true)?;
    let state = grafeno_server::http::AppState::new(db, token.as_deref().filter(|t| !t.is_empty()));
    let options = grafeno_server::http::Options { cors_origins };
    std::thread::Builder::new()
        .name("grafeno-http".into())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(error) => {
                        eprintln!("grafeno: cannot serve the HTTP API: {error}");
                        return;
                    }
                };
                if let Err(error) =
                    grafeno_server::http::serve_on(state, listener, options, std::future::pending())
                        .await
                {
                    eprintln!("grafeno: the HTTP API stopped: {error}");
                }
            });
        })?;
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("grafeno: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Serve {
            config,
            db,
            memory,
            addr,
            token,
            no_studio,
            cors_origins,
            stop_with_stdin,
        } => {
            let config = config.map(|path| Config::load(&path)).transpose()?;
            let overrides = Overrides {
                db,
                memory,
                addr,
                token,
                no_studio,
                cors_origins,
            };
            let settings = ServerSettings::resolve(config, overrides);
            if stop_with_stdin {
                service::run_until_signal_or_stdin(settings)?;
            } else {
                service::run_until_signal(settings)?;
            }
        }
        Command::Config(command) => match command {
            ConfigCommand::Init {
                path,
                db,
                addr,
                force,
            } => {
                let path = path.unwrap_or_else(config::default_path);
                let db = db.unwrap_or_else(|| {
                    path.parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join("data")
                        .join("grafeno.redb")
                });
                config::init(&path, &db, addr, force)?;
                println!("Wrote {}.", path.display());
            }
            ConfigCommand::Path => println!("{}", config::default_path().display()),
            ConfigCommand::Token { path } => {
                let path = path.unwrap_or_else(config::default_path);
                match Config::load(&path)?.token {
                    Some(token) => println!("{token}"),
                    None => return Err(format!("{} has no token", path.display()).into()),
                }
            }
        },
        Command::Service(command) => service_command(command)?,
        Command::Setup(args) => setup_command(args)?,
        Command::Instances { command } => {
            use grafeno_server::instances::{self, Instance};
            match command.unwrap_or(InstancesCommand::List) {
                InstancesCommand::List => {
                    let all = instances::list();
                    if all.is_empty() {
                        println!(
                            "No instances in {} (create one with grafeno setup).",
                            instances::config_dir().display()
                        );
                    } else {
                        println!("{:12} {:22} {:24} DATABASE", "NAME", "ADDRESS", "SERVICE");
                        for instance in &all {
                            println!("{}", instances::describe(instance));
                        }
                    }
                }
                InstancesCommand::Drop { name, purge, yes } => {
                    let instance = Instance::named(&name)?;
                    if !yes {
                        let what = if purge {
                            "its service, settings AND DATA"
                        } else {
                            "its service and settings (the data is kept)"
                        };
                        eprint!("Remove the instance {name:?}, {what}? Type its name to confirm: ");
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        if line.trim() != name {
                            return Err("not confirmed; nothing was removed".into());
                        }
                    }
                    instances::drop_instance(&instance, purge)?;
                }
                InstancesCommand::Rollback { name } => {
                    instances::rollback(&Instance::named(&name)?)?;
                }
            }
        }
        Command::Doctor { instance } => {
            use grafeno_server::instances::{self, Instance};
            let all = match instance {
                Some(name) => vec![Instance::named(&name)?],
                None => instances::list(),
            };
            if all.is_empty() {
                return Err(format!(
                    "no instances in {} (create one with grafeno setup)",
                    instances::config_dir().display()
                )
                .into());
            }
            let mut failed = false;
            for instance in &all {
                let findings = instances::doctor(instance);
                failed |= instances::print_findings(instance, &findings);
            }
            if failed {
                return Err("some checks failed".into());
            }
        }
        Command::Mcp {
            db,
            namespace,
            allow,
            read_only,
            actor,
            own_only,
            listen,
            token,
            cors_origins,
        } => {
            if let Some(name) = &actor {
                grafeno::check_principal_name(name)?;
            }
            let access = mcp::McpAccess {
                allow: allow.into_iter().filter(|a| !a.trim().is_empty()).collect(),
                read_only,
                actor: actor.map(|name| grafeno::Actor {
                    name,
                    own_only,
                    view: None,
                }),
            };
            let db = std::sync::Arc::new(db.open()?);
            if let Some(addr) = listen {
                serve_beside_mcp(db.clone(), addr, token, cors_origins)?;
            }
            mcp::run_shared(db, namespace, access)?
        }
        Command::Shell {
            config,
            db,
            memory,
            url,
            token,
            user,
            password_stdin,
            namespace,
            command,
            file,
            json,
            stop_on_error,
        } => {
            let (url, token) = match config {
                Some(path) => {
                    let config = Config::load(&path)?;
                    (Some(config.local_url()), token.or(config.token))
                }
                None => (url, token),
            };
            let backend = match url {
                Some(url) => Backend::remote(&url, token.filter(|t| !t.is_empty())),
                None if memory => Backend::Local(Grafeno::in_memory()?),
                None => {
                    let path = db.unwrap_or_else(|| PathBuf::from("grafeno.redb"));
                    Backend::Local(
                        Grafeno::open(&path).map_err(|error| held_elsewhere(&path, error))?,
                    )
                }
            };
            if let Some(user) = &user {
                let password = if password_stdin {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\r', '\n']).to_owned()
                } else {
                    rpassword::prompt_password(format!("Password for {user}: "))?
                };
                let name = backend.sign_in(user, &password)?;
                eprintln!("Signed in as {name}.");
            }
            let mut shell = Shell::new(backend, namespace, json);
            shell.stop_on_error = stop_on_error;
            if let Some(command) = command {
                shell.run_script(&command);
            } else if let Some(file) = file {
                shell.run_script(&read_script(&file)?);
            } else if let Some(script) = stdin_script() {
                shell.run_script(&script);
            } else {
                shell.interactive()?;
            }
            if shell.failed {
                return Err("some statements failed".into());
            }
        }
        Command::Backup { output, target } => {
            if output.exists() {
                return Err(format!("{} exists already", output.display()).into());
            }
            match &target.config {
                Some(config) => {
                    let config = Config::load(config)?;
                    let agent = ureq::Agent::config_builder()
                        .http_status_as_error(false)
                        .build()
                        .new_agent();
                    let mut request = agent.get(format!("{}/v1/backup", config.local_url()));
                    if let Some(token) = &config.token {
                        request = request.header("authorization", format!("Bearer {token}"));
                    }
                    let mut response = request.call()?;
                    if !response.status().is_success() {
                        let text = response.body_mut().read_to_string().unwrap_or_default();
                        return Err(format!("the server refused the backup: {text}").into());
                    }
                    let bytes = response
                        .body_mut()
                        .with_config()
                        .limit(u64::MAX)
                        .read_to_vec()?;
                    std::fs::write(&output, &bytes)?;
                    println!(
                        "Backup written to {} ({} bytes).",
                        output.display(),
                        bytes.len()
                    );
                }
                None => {
                    let stats = Grafeno::open(&target.db)?.begin_read()?.backup(&output)?;
                    println!(
                        "Backup written to {} ({} entries, {} bytes).",
                        output.display(),
                        stats.entries,
                        stats.bytes
                    );
                }
            }
        }
        Command::Restore { backup, output } => {
            let bytes = Grafeno::recover_backup(&backup, &output)?;
            println!(
                "Restored {} into {} ({bytes} bytes).",
                backup.display(),
                output.display()
            );
        }
        Command::Cluster(command) => match command {
            ClusterCommand::Status { target } => cluster_call(&target, "GET", "", None)?,
            ClusterCommand::Voters { target, ids } => cluster_call(
                &target,
                "POST",
                "/voters",
                Some(serde_json::json!({ "voters": ids })),
            )?,
            ClusterCommand::Compact { target } => cluster_call(&target, "POST", "/compact", None)?,
        },
        Command::Keys(command) => {
            use grafeno_server::keys;
            match command {
                KeysCommand::List { target } => keys::list(&target.resolve()?)?,
                KeysCommand::Create {
                    target,
                    name,
                    grants,
                    owner,
                    own_only,
                    description,
                } => {
                    let grants = grants
                        .iter()
                        .map(|grant| keys::parse_grant(grant))
                        .collect::<Result<Vec<_>, _>>()?;
                    keys::create(
                        &target.resolve()?,
                        &name,
                        grants,
                        owner,
                        own_only,
                        description,
                    )?
                }
                KeysCommand::Revoke { target, name } => keys::revoke(&target.resolve()?, &name)?,
            }
        }
        Command::Users(command) => {
            use grafeno_server::{keys, users};
            match command {
                UsersCommand::List { target } => users::list(&target.resolve()?)?,
                UsersCommand::Create {
                    target,
                    name,
                    superuser,
                    grants,
                    service,
                    own_only,
                    password_stdin,
                } => {
                    let grants = grants
                        .iter()
                        .map(|grant| keys::parse_grant(grant))
                        .collect::<Result<Vec<_>, _>>()?;
                    let password = if service {
                        None
                    } else {
                        Some(users::read_password(&name, password_stdin)?)
                    };
                    users::create(
                        &target.resolve()?,
                        users::NewAccount {
                            name,
                            service,
                            superuser,
                            grants,
                            own_only,
                            password,
                        },
                    )?
                }
                UsersCommand::Password {
                    target,
                    name,
                    password_stdin,
                } => {
                    let password = users::read_password(&name, password_stdin)?;
                    users::change(&target.resolve()?, &name, Some(password), None)?
                }
                UsersCommand::Disable { target, name } => {
                    users::change(&target.resolve()?, &name, None, Some(true))?
                }
                UsersCommand::Enable { target, name } => {
                    users::change(&target.resolve()?, &name, None, Some(false))?
                }
                UsersCommand::Delete { target, name } => users::delete(&target.resolve()?, &name)?,
            }
        }
        Command::Roles(command) => {
            use grafeno_server::access_cli::{quote, run};
            match command {
                RolesCommand::List { target } => run(&target.resolve()?, "SHOW ROLES")?,
                RolesCommand::Create {
                    target,
                    name,
                    description,
                } => {
                    let mut statement = format!("CREATE ROLE {}", quote(&name));
                    if let Some(description) = description {
                        statement.push_str(&format!(" SET DESCRIPTION {}", quote(&description)));
                    }
                    run(&target.resolve()?, &statement)?
                }
                RolesCommand::Delete { target, name } => {
                    run(&target.resolve()?, &format!("DROP ROLE {}", quote(&name)))?
                }
                RolesCommand::Show { target, name } => run(
                    &target.resolve()?,
                    &format!("SHOW PRIVILEGES FOR {}", quote(&name)),
                )?,
            }
        }
        Command::Grants(command) => {
            use grafeno_server::access_cli::{privileges, quote, run, target as on};
            match command {
                GrantsCommand::Catalog { target } => run(&target.resolve()?, "SHOW PRIVILEGES")?,
                GrantsCommand::Grant {
                    target,
                    privileges: list,
                    on: object,
                    to,
                } => run(
                    &target.resolve()?,
                    &format!(
                        "GRANT {} ON {} TO {}",
                        privileges(&list)?,
                        on(&object)?,
                        quote(&to)
                    ),
                )?,
                GrantsCommand::Deny {
                    target,
                    privileges: list,
                    on: object,
                    to,
                } => run(
                    &target.resolve()?,
                    &format!(
                        "DENY {} ON {} TO {}",
                        privileges(&list)?,
                        on(&object)?,
                        quote(&to)
                    ),
                )?,
                GrantsCommand::Revoke {
                    target,
                    privileges: list,
                    on: object,
                    from,
                    grants,
                    denials,
                } => {
                    let which = if grants {
                        "GRANT "
                    } else if denials {
                        "DENY "
                    } else {
                        ""
                    };
                    run(
                        &target.resolve()?,
                        &format!(
                            "REVOKE {which}{} ON {} FROM {}",
                            privileges(&list)?,
                            on(&object)?,
                            quote(&from)
                        ),
                    )?
                }
                GrantsCommand::AddRole { target, role, to } => run(
                    &target.resolve()?,
                    &format!("GRANT ROLE {} TO {}", quote(&role), quote(&to)),
                )?,
                GrantsCommand::RemoveRole { target, role, from } => run(
                    &target.resolve()?,
                    &format!("REVOKE ROLE {} FROM {}", quote(&role), quote(&from)),
                )?,
            }
        }
        Command::Namespaces {
            db,
            action: Some(action),
        } => {
            let db = db.open()?;
            match action {
                NamespacesCommand::Create {
                    name,
                    description,
                    if_not_exists,
                } => {
                    let exists = db.begin_read()?.namespace(&name)?.is_some();
                    if exists && if_not_exists {
                        println!("namespace {name:?} exists already");
                    } else {
                        db.write(|tx| tx.create_namespace(&name, description.as_deref()))?;
                        println!("created namespace {name:?}");
                    }
                }
                NamespacesCommand::Describe { name, description } => {
                    if db.begin_read()?.namespace(&name)?.is_none() {
                        return Err(format!("no namespace {name:?}").into());
                    }
                    db.write(|tx| tx.describe_namespace(&name, description.as_deref()))?;
                    println!("described namespace {name:?}");
                }
                NamespacesCommand::Drop { name, yes } => {
                    if db.begin_read()?.namespace(&name)?.is_none() {
                        return Err(format!("no namespace {name:?}").into());
                    }
                    if !yes {
                        return Err(format!(
                            "dropping {name:?} deletes everything in it: add --yes to confirm"
                        )
                        .into());
                    }
                    let dropped = db.write(|tx| tx.drop_namespace(&name))?;
                    println!(
                        "dropped namespace {name:?}: {} nodes and {} edges",
                        dropped.nodes, dropped.edges
                    );
                }
            }
        }
        Command::Namespaces { db, action: None } => {
            let namespaces = db.open()?.begin_read()?.namespaces()?;
            let width = namespaces
                .iter()
                .map(|info| info.name.len())
                .max()
                .unwrap_or(0)
                .max(9);
            println!("{:width$}  {:>10}  {:>10}", "namespace", "nodes", "edges");
            for info in namespaces {
                println!(
                    "{:width$}  {:>10}  {:>10}",
                    info.name, info.nodes, info.edges
                );
            }
        }
        Command::Stats { db, namespaces } => {
            let db = db.open()?;
            let read = db.begin_read()?;
            let stats = namespaces_or_all(&db, namespaces)?
                .iter()
                .map(|ns| read.stats(ns))
                .collect::<grafeno::Result<Vec<_>>>()?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
        }
        Command::Extract {
            db,
            namespaces,
            all: _,
            force,
        } => {
            let db = db.open()?;
            let options = grafeno_memory::ExtractOptions {
                force,
                ..grafeno_memory::ExtractOptions::default()
            };
            for ns in namespaces_or_all(&db, namespaces)? {
                let report = grafeno_memory::extract(&db, &ns, &options)?;
                let waiting =
                    grafeno_memory::review_queue(&db.begin_read()?, &ns, usize::MAX)?.len();
                println!(
                    "{ns}: {} texts read ({} unchanged), {} mentions, {} entities created, {waiting} waiting for review",
                    report.read, report.unchanged, report.mentions, report.created
                );
            }
        }
        Command::Compact {
            db,
            namespaces,
            all: _,
        } => {
            let db = db.open()?;
            for ns in namespaces_or_all(&db, namespaces)? {
                // The namespace's own graph, then each vector index.
                let graphs = db.begin_read()?.vector_health(&ns)?;
                let graphs: Vec<Option<String>> = if graphs.is_empty() {
                    vec![None]
                } else {
                    graphs
                        .into_iter()
                        .filter(|h| !h.building)
                        .map(|h| h.index)
                        .collect()
                };
                for index in graphs {
                    let report = match &index {
                        Some(name) => db.write(|tx| tx.compact_vector_index(&ns, name))?,
                        None => db.write(|tx| tx.compact_vectors(&ns))?,
                    };
                    let what = match &index {
                        Some(name) => format!("{ns} (vector index {name})"),
                        None => ns.clone(),
                    };
                    println!(
                        "{what}: {} vectors; {} unreachable and {} dangling links before, {} unreachable after",
                        report.vectors,
                        report.unreachable_before,
                        report.dangling_before,
                        report.unreachable_after
                    );
                }
            }
        }
        Command::Check {
            db,
            namespaces,
            recall,
        } => {
            // First every page against its checksum: reading a damaged
            // page would stop this program, not report it.
            match Grafeno::verify_file(&db.db)? {
                grafeno::FileIntegrity::Clean => println!("file: every page matches its checksum"),
                grafeno::FileIntegrity::Repaired => {
                    println!("file: damage found and repaired by redb (recent commits may be lost)")
                }
            }
            let db = db.open()?;
            let read = db.begin_read()?;
            let mut failed = 0;
            for ns in namespaces_or_all(&db, namespaces)? {
                let report = read.check(&ns)?;
                let summary = format!(
                    "{} nodes, {} edges, {} texts, {} vectors, {} dangling links",
                    report.nodes, report.edges, report.texts, report.vectors, report.dangling_links
                );
                let summary = if report.unreachable_vectors > 0 {
                    format!(
                        "{summary}, {} vectors searches cannot reach (`grafeno compact` reconnects them)",
                        report.unreachable_vectors
                    )
                } else {
                    summary
                };
                // Removals since each graph was built: past about half,
                // searches can slip and compacting is worth its time.
                let mut summary = summary;
                for health in read.vector_health(&ns)? {
                    if health.removed > 0 {
                        let graph = match &health.index {
                            Some(name) => format!("vector index {name}"),
                            None => "the vector graph".to_owned(),
                        };
                        summary = format!(
                            "{summary}, {} removals since {graph} was built ({:.0}% of its {} vectors)",
                            health.removed,
                            health.churn * 100.0,
                            health.vectors
                        );
                    }
                }
                let summary = match recall {
                    Some(sample) if report.vectors > 0 => {
                        match read.vector_recall(&ns, sample, 64)? {
                            Some(value) => {
                                format!("{summary}, vector recall@10 about {value:.3} (ef 64)")
                            }
                            None => summary,
                        }
                    }
                    _ => summary,
                };
                if report.is_ok() {
                    println!("{ns}: ok ({summary})");
                } else {
                    failed += 1;
                    println!("{ns}: {} problems ({summary})", report.problems.len());
                    for problem in &report.problems {
                        println!("  - {problem}");
                    }
                }
            }
            if failed > 0 {
                return Err(format!(
                    "{failed} namespace(s) have problems; `grafeno rebuild` can repair indexes"
                )
                .into());
            }
        }
        Command::Rebuild {
            db,
            namespaces,
            all: _,
        } => {
            let db = db.open()?;
            for ns in namespaces_or_all(&db, namespaces)? {
                let report = db.write(|tx| tx.rebuild_indexes(&ns))?;
                println!("{}", serde_json::to_string(&report)?);
            }
        }
        Command::Export {
            db,
            namespaces,
            output,
            format,
        } if format != Format::Jsonl => {
            let [ns] = namespaces.as_slice() else {
                return Err("CSV and GraphML export one namespace: give --namespace".into());
            };
            let db = db.open()?;
            let read = db.begin_read()?;
            if format == Format::Csv {
                let folder =
                    output.ok_or("CSV needs --output, a folder for nodes.csv and edges.csv")?;
                std::fs::create_dir_all(&folder)?;
                let mut nodes =
                    std::io::BufWriter::new(std::fs::File::create(folder.join("nodes.csv"))?);
                let mut edges =
                    std::io::BufWriter::new(std::fs::File::create(folder.join("edges.csv"))?);
                let n = read.export_nodes_csv(ns, &mut nodes)?;
                let e = read.export_edges_csv(ns, &mut edges)?;
                eprintln!("exported {n} nodes and {e} edges to {}", folder.display());
            } else {
                let (n, e) = match output {
                    Some(path) => read.export_graphml(
                        ns,
                        &mut std::io::BufWriter::new(std::fs::File::create(path)?),
                    )?,
                    None => read.export_graphml(ns, &mut std::io::stdout().lock())?,
                };
                eprintln!("exported {n} nodes and {e} edges");
            }
        }
        Command::Import {
            db,
            input,
            format,
            namespace,
            edges,
            ..
        } if format != Format::Jsonl => {
            let ns = namespace.ok_or("CSV and GraphML go into one namespace: give --namespace")?;
            let db = db.open()?;
            let options = grafeno::InterchangeOptions::default();
            let open = |path: &PathBuf| -> std::io::Result<Box<dyn std::io::BufRead>> {
                Ok(if path.as_os_str() == "-" {
                    Box::new(std::io::BufReader::new(std::io::stdin()))
                } else {
                    Box::new(std::io::BufReader::new(std::fs::File::open(path)?))
                })
            };
            let mut stats = if format == Format::Csv {
                db.import_nodes_csv(&ns, open(&input)?, &options)?
            } else {
                db.import_graphml(&ns, open(&input)?, &options)?
            };
            if let Some(edges) = edges {
                let more = db.import_edges_csv(&ns, open(&edges)?, &options)?;
                stats.edges += more.edges;
                stats.failed += more.failed;
                stats.errors.extend(more.errors);
            }
            for error in &stats.errors {
                eprintln!("skipped: {error}");
            }
            eprintln!(
                "imported {} nodes and {} edges into {ns}; {} skipped",
                stats.nodes, stats.edges, stats.failed
            );
        }
        Command::Export {
            db,
            namespaces,
            output,
            ..
        } => {
            let db = db.open()?;
            let read = db.begin_read()?;
            let stats = match output {
                Some(path) => {
                    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
                    read.export(&namespaces, &mut file)?
                }
                None => read.export(&namespaces, &mut std::io::stdout().lock())?,
            };
            eprintln!(
                "exported {} namespaces, {} nodes, {} edges",
                stats.namespaces, stats.nodes, stats.edges
            );
        }
        Command::Import {
            db, input, bulk, ..
        } => {
            let db = db.open()?;
            let options = if bulk {
                grafeno::ImportOptions::bulk()
            } else {
                grafeno::ImportOptions::default()
            };
            let stats = if input.as_os_str() == "-" {
                db.import_with(std::io::stdin().lock(), &options)?
            } else {
                db.import_with(
                    std::io::BufReader::new(std::fs::File::open(input)?),
                    &options,
                )?
            };
            eprintln!(
                "imported {} namespaces, {} nodes, {} edges",
                stats.namespaces, stats.nodes, stats.edges
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn service_command(command: ServiceCommand) -> Result<(), Box<dyn std::error::Error>> {
    use grafeno_server::service::windows;
    match command {
        ServiceCommand::Install {
            config,
            no_start,
            account,
        } => {
            let config = config.unwrap_or_else(config::default_path);
            if !config.exists() {
                return Err(format!(
                    "{} does not exist; create it with `grafeno config init`",
                    config.display()
                )
                .into());
            }
            windows::install(&config, !no_start, account.as_deref())?;
            println!(
                "Installed the {} service{}.",
                windows::NAME,
                if no_start { "" } else { " and started it" }
            );
        }
        ServiceCommand::Uninstall => {
            windows::uninstall()?;
            println!("Removed the {} service.", windows::NAME);
        }
        ServiceCommand::Run { config } => {
            windows::dispatch(config.unwrap_or_else(config::default_path))?;
        }
    }
    Ok(())
}

fn setup_command(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    use grafeno_server::instances::{self, Answers, SetupMode};
    let durability = match args.durability.as_deref() {
        None => None,
        Some("immediate") => Some(config::DurabilitySetting::Immediate),
        Some("eventual") => Some(config::DurabilitySetting::Eventual),
        Some(_) => return Err("--durability is immediate or eventual".into()),
    };
    let answers = Answers {
        instance: args.instance,
        data: args.data,
        addr: args.addr,
        port: args.port,
        network: args.network.then_some(true),
        durability,
        memory_mb: args.memory_mb,
        admin: args.admin,
        admin_password_file: args.admin_password_file,
        no_admin: args.no_admin,
        start: args.no_start.then_some(false),
    }
    .merged_over(args.answers.as_deref())?;
    instances::setup(
        answers,
        &SetupMode {
            interactive: !args.unattended,
            password_stdin: args.admin_password_stdin,
            quiet: args.quiet,
        },
    )?;
    Ok(())
}

#[cfg(not(windows))]
fn service_command(command: ServiceCommand) -> Result<(), Box<dyn std::error::Error>> {
    let _ = command;
    Err(
        "`grafeno service` is for Windows. On Linux the packages install a systemd unit: \
         `systemctl enable --now grafeno` (or use install.sh --service)."
            .into(),
    )
}
