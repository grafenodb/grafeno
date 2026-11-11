//! Who is calling, and what it may do (phase 14.1).
//!
//! Callers are accounts ([`grafeno::Account`]): users, who sign in with a
//! password and get a session, and service accounts, which use API keys.
//! A request carries a session's access token (`gfs_<id>.<secret>`) or an
//! API key (`gfk_…`), whose account's grants (narrowed by the key's scope)
//! decide what it may do. The server's static token, when one is
//! configured, is a break-glass credential with every right. A server with
//! neither a token nor accounts is open, as on a developer's machine, and
//! the first sign-in there creates its first superuser.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use grafeno::{Account, Act, Actor, Grafeno, KeyScope, Role};
use sha2::{Digest, Sha256};

/// The hex SHA-256 of a secret, as stored.
pub fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).expect("the system has randomness");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new API key secret: `gfk_` and 40 random hex digits.
pub fn new_secret() -> String {
    format!("gfk_{}", random_hex(20))
}

/// A session's access token starts with this.
pub const ACCESS_PREFIX: &str = "gfs_";
/// A session's refresh token starts with this.
pub const REFRESH_PREFIX: &str = "gfr_";

/// A new session id.
pub fn new_session_id() -> String {
    random_hex(12)
}

/// A new token of a session: `<prefix><id>.<secret>`, and its secret.
pub fn new_token(prefix: &str, id: &str) -> (String, String) {
    let secret = random_hex(24);
    (format!("{prefix}{id}.{secret}"), secret)
}

/// The session id and secret of a token with `prefix`.
pub fn split_token<'t>(token: &'t str, prefix: &str) -> Option<(&'t str, &'t str)> {
    token.strip_prefix(prefix)?.split_once('.')
}

// ------------------------------------------------------------- passwords

/// Argon2id with the parameters OWASP recommends (19 MiB, 2 passes, 1
/// lane): about 19 ms a hash on the laptop of the measurements.
fn argon() -> argon2::Argon2<'static> {
    argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(19 * 1024, 2, 1, None).expect("valid parameters"),
    )
}

/// A password's Argon2id hash, in PHC format.
pub fn hash_password(password: &str) -> String {
    use argon2::password_hash::{PasswordHasher, SaltString};
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("the system has randomness");
    let salt = SaltString::encode_b64(&salt).expect("a valid salt");
    argon()
        .hash_password(password.as_bytes(), &salt)
        .expect("hashing works")
        .to_string()
}

/// Whether `password` is the one `hash` was made from.
pub fn verify_password(hash: &str, password: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    PasswordHash::new(hash).is_ok_and(|parsed| {
        argon()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// A check run when no password matches, so that a wrong name takes as
/// long as a wrong password.
pub fn verify_nothing(password: &str) {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let hash = DUMMY.get_or_init(|| hash_password("not a password anyone has"));
    let _ = verify_password(hash, password);
}

/// Passwords too common to accept (the start of the usual lists).
const COMMON: &[&str] = &[
    "password",
    "password1",
    "password12",
    "password123",
    "passw0rd",
    "123456789",
    "1234567890",
    "12345678910",
    "qwertyuiop",
    "qwerty123",
    "qwerty1234",
    "1q2w3e4r5t",
    "1qaz2wsx3edc",
    "iloveyou",
    "iloveyou1",
    "sunshine1",
    "princess1",
    "football1",
    "baseball1",
    "welcome1",
    "welcome123",
    "abc1234567",
    "abcdefghij",
    "letmein123",
    "monkey1234",
    "dragon1234",
    "trustno1234",
    "superman12",
    "1111111111",
    "0000000000",
    "administrator",
    "admin12345",
    "changeme123",
    "changeme",
    "contrasena",
    "contraseña",
    "contraseña1",
    "123123123123",
    "azertyuiop",
    "zaq12wsxcde",
    "q1w2e3r4t5",
    "asdfghjkl1",
    "starwars12",
    "computer12",
];

/// The shortest password accepted.
pub const MIN_PASSWORD: usize = 10;

/// Why a password is refused, if it is.
pub fn check_password(name: &str, password: &str) -> Result<(), String> {
    let length = password.chars().count();
    if length < MIN_PASSWORD {
        return Err(format!("a password has at least {MIN_PASSWORD} characters"));
    }
    if length > 128 {
        return Err("a password has at most 128 characters".into());
    }
    let lower = password.to_lowercase();
    if lower.contains(&name.to_lowercase()) && name.len() >= 3 {
        return Err("a password must not contain the account's name".into());
    }
    if COMMON.contains(&lower.as_str())
        || lower
            .chars()
            .all(|c| c == lower.chars().next().unwrap_or(' '))
    {
        return Err("this password is too common: choose another".into());
    }
    Ok(())
}

// -------------------------------------------------------------- attempts

/// Failed sign-ins, per account and per address, kept by this member: an
/// account is locked for a while after too many, and an address refused.
#[derive(Default)]
pub struct Throttle {
    accounts: Mutex<HashMap<String, Vec<Instant>>>,
    addresses: Mutex<HashMap<String, Vec<Instant>>>,
}

/// How many failures lock an account or an address, and for how long.
#[derive(Clone, Copy, Debug)]
pub struct Lockout {
    pub account_attempts: usize,
    pub address_attempts: usize,
    pub window: Duration,
}

impl Default for Lockout {
    fn default() -> Self {
        Self {
            account_attempts: 5,
            address_attempts: 20,
            window: Duration::from_secs(15 * 60),
        }
    }
}

fn recent(map: &Mutex<HashMap<String, Vec<Instant>>>, key: &str, window: Duration) -> usize {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let entry = map.entry(key.to_owned()).or_default();
    entry.retain(|at| now.duration_since(*at) < window);
    entry.len()
}

impl Throttle {
    /// Why a sign-in is refused before its password is checked: the
    /// account is locked, or the address made too many attempts.
    pub fn check(
        &self,
        lockout: Lockout,
        account: &str,
        address: Option<&str>,
    ) -> Result<(), String> {
        if let Some(address) = address
            && recent(&self.addresses, address, lockout.window) >= lockout.address_attempts
        {
            return Err("too many failed sign-ins from this address: try again later".into());
        }
        if recent(&self.accounts, account, lockout.window) >= lockout.account_attempts {
            return Err(format!(
                "the account is locked after {} failed sign-ins: try again in {} minutes",
                lockout.account_attempts,
                lockout.window.as_secs().div_ceil(60)
            ));
        }
        Ok(())
    }

    pub fn failed(&self, account: &str, address: Option<&str>) {
        let now = Instant::now();
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account.to_owned())
            .or_default()
            .push(now);
        if let Some(address) = address {
            self.addresses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(address.to_owned())
                .or_default()
                .push(now);
        }
    }

    pub fn succeeded(&self, account: &str) {
        self.accounts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(account);
    }
}

// ------------------------------------------------------------ credentials

/// Which key a secret belongs to, by its hash: kept in memory, read again
/// when a secret is not found (keys created on another cluster member
/// arrive through the log), at most every 200 ms.
pub struct Credentials {
    by_hash: RwLock<HashMap<String, String>>,
    reloaded: Mutex<Option<Instant>>,
    accounts: std::sync::atomic::AtomicBool,
    /// Each account's effective rights (14.2), with the access generation
    /// they were computed at: any change of accounts or roles bumps it.
    rights: RwLock<HashMap<String, (u64, Arc<grafeno::View>)>>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            by_hash: RwLock::default(),
            reloaded: Mutex::new(None),
            accounts: std::sync::atomic::AtomicBool::new(false),
            rights: RwLock::default(),
        }
    }
}

impl Credentials {
    /// `account`'s rights as of `read`, as a view of the records it may
    /// see and change (14.4): cached until accounts or roles change.
    pub fn view(
        &self,
        read: &grafeno::ReadTx,
        account: &Account,
    ) -> grafeno::Result<Arc<grafeno::View>> {
        let generation = read.access_generation()?;
        if let Some((at, view)) = self
            .rights
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&account.name)
            && *at == generation
        {
            return Ok(view.clone());
        }
        let view = Arc::new(grafeno::View::new(
            Arc::new(read.rights(account)?),
            Some(account),
        ));
        let mut cache = self.rights.write().unwrap_or_else(|e| e.into_inner());
        // Entries of an older generation are all stale.
        cache.retain(|_, (at, _)| *at >= generation);
        cache.insert(account.name.clone(), (generation, view.clone()));
        Ok(view)
    }

    pub fn load(db: &Grafeno) -> grafeno::Result<Self> {
        let credentials = Self::default();
        credentials.reload(db)?;
        Ok(credentials)
    }

    /// Reads the keys again (after one was created, rotated or revoked).
    pub fn reload(&self, db: &Grafeno) -> grafeno::Result<()> {
        let read = db.begin_read()?;
        let map = read
            .api_keys()?
            .into_iter()
            .map(|key| (key.secret_hash, key.name))
            .collect();
        let any = !read.accounts()?.is_empty();
        *self.by_hash.write().unwrap_or_else(|e| e.into_inner()) = map;
        self.accounts
            .store(any, std::sync::atomic::Ordering::Relaxed);
        *self.reloaded.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        Ok(())
    }

    /// Whether the database has accounts (an open server has none).
    pub fn has_accounts(&self) -> bool {
        self.accounts.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Notes that an account now exists.
    pub fn note_account(&self) {
        self.accounts
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The name of the key whose secret hashes to `hash`.
    pub fn key_name(&self, db: &Grafeno, hash: &str) -> Option<String> {
        if let Some(name) = self
            .by_hash
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(hash)
        {
            return Some(name.clone());
        }
        let stale = self
            .reloaded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(200));
        if stale && self.reload(db).is_ok() {
            return self
                .by_hash
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(hash)
                .cloned();
        }
        None
    }
}

/// How a caller proved who it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    Key(String),
    Session(String),
}

/// The caller of a request.
#[derive(Clone, Debug)]
pub enum Caller {
    /// The static token, or an open server: everything.
    Owner,
    /// Nobody yet (signing in): nothing.
    Anonymous,
    Account {
        account: Arc<Account>,
        /// Its effective rights (its grants and roles), record by record.
        view: Arc<grafeno::View>,
        /// A key's scope, narrowing the account's rights.
        scope: Option<KeyScope>,
        via: Via,
    },
}

impl Caller {
    /// May the caller do `action` in `ns`?
    pub fn can(&self, action: Act, ns: &str) -> bool {
        match self {
            Caller::Owner => true,
            Caller::Anonymous => false,
            Caller::Account { view, scope, .. } => {
                let rights = &view.rights;
                // Label and property rules let a request through; each record
                // is then checked (14.4).
                let by_record = match action {
                    Act::Read => rights.per_record(ns),
                    Act::Create | Act::Update | Act::Delete | Act::Merge | Act::Retract => {
                        rights.writes_labels_in(ns)
                    }
                    _ => false,
                };
                (rights.on_namespace(action, ns) || by_record)
                    && scope
                        .as_ref()
                        .is_none_or(|scope| scope_allows(scope, action, ns))
            }
        }
    }

    /// May the caller do every one of `actions` in `ns`?
    pub fn can_all(&self, actions: &[Act], ns: &str) -> bool {
        actions.iter().all(|action| self.can(*action, ns))
    }

    /// May the caller do `action` on the server? A scoped key never may.
    pub fn can_server(&self, action: Act) -> bool {
        match self {
            Caller::Owner => true,
            Caller::Anonymous => false,
            Caller::Account { view, scope, .. } => scope.is_none() && view.rights.on_server(action),
        }
    }

    /// What a built-in level stood for before 14.2 (see [`role_actions`]).
    pub fn may(&self, ns: &str, role: Role) -> bool {
        self.can_all(role_actions(role), ns)
    }

    /// Every right: the static token, an open server, or a superuser not
    /// narrowed by a key's scope.
    pub fn is_owner(&self) -> bool {
        match self {
            Caller::Owner => true,
            Caller::Anonymous => false,
            Caller::Account { account, scope, .. } => account.superuser && scope.is_none(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Caller::Owner => "owner",
            Caller::Anonymous => "anonymous",
            Caller::Account { account, .. } => &account.name,
        }
    }

    /// What the caller may see and change record by record (`None`: every
    /// record, the owner's token or a superuser).
    pub fn view(&self) -> Option<Arc<grafeno::View>> {
        match self {
            Caller::Account { view, .. } if !view.rights.superuser => Some(view.clone()),
            _ => None,
        }
    }

    /// The account behind the caller, if any.
    pub fn account(&self) -> Option<&Arc<Account>> {
        match self {
            Caller::Account { account, .. } => Some(account),
            _ => None,
        }
    }

    /// Who the engine records as the writer (nobody for the owner token).
    pub fn actor(&self) -> Option<Actor> {
        match self {
            Caller::Owner | Caller::Anonymous => None,
            Caller::Account { account, view, .. } => Some(Actor {
                name: account.name.clone(),
                own_only: account.own_only,
                view: (!view.rights.superuser).then(|| view.clone()),
            }),
        }
    }

    /// Why a request is refused: one of `actions` on `ns` is missing.
    pub fn refusal(&self, ns: &str, actions: &[Act]) -> String {
        let missing = actions
            .iter()
            .find(|action| !self.can(**action, ns))
            .or(actions.first())
            .map(|action| action.name())
            .unwrap_or_default();
        let role = format!("`{missing}`");
        match self {
            Caller::Account {
                via: Via::Key(key),
                scope: Some(_),
                ..
            } => format!(
                "the key {key:?} (of {:?}) has no {role} privilege on the namespace {ns:?} within its scope",
                self.name()
            ),
            _ => format!(
                "the account {:?} has no {role} privilege on the namespace {ns:?}",
                self.name()
            ),
        }
    }

    /// Why a request is refused: `action` on the server is missing.
    pub fn server_refusal(&self, action: Act) -> String {
        match self {
            Caller::Account { scope: Some(_), .. } => format!(
                "{:?} uses a scoped key: server privileges (`{}`) need an unscoped one",
                self.name(),
                action.name()
            ),
            _ => format!(
                "the account {:?} has no `{}` privilege on the server",
                self.name(),
                action.name()
            ),
        }
    }
}

/// What the caller may manage, for access statements in GQ (14.3); passwords
/// set there go through the policy and Argon2id.
pub fn access_context(caller: &Caller) -> grafeno_query::AccessContext {
    let hasher: grafeno_query::PasswordHasher = Arc::new(|name: &str, password: &str| {
        check_password(name, password)?;
        Ok(hash_password(password))
    });
    grafeno_query::AccessContext {
        account: caller.account().map(|a| a.name.clone()),
        superuser: caller.is_owner(),
        manage_accounts: caller.can_server(Act::ManageAccounts),
        manage_roles: caller.can_server(Act::ManageRoles),
        read_audit: caller.can_server(Act::Audit),
        effective_roles: match caller {
            Caller::Account { view, .. } => view.rights.roles.clone(),
            _ => Vec::new(),
        },
        hash_password: Some(hasher),
    }
}

/// The actions a built-in level stands for in a check: reading is `read`,
/// writing is creating, updating and deleting, administering is changing
/// the schema.
pub fn role_actions(role: Role) -> &'static [Act] {
    match role {
        Role::Read => &[Act::Read],
        Role::Write => &[Act::Create, Act::Update, Act::Delete],
        Role::Admin => &[Act::Schema],
    }
}

/// Whether a key's scope lets `action` in `ns` through: only its
/// namespaces, and only reading if it is read-only.
fn scope_allows(scope: &KeyScope, action: Act, ns: &str) -> bool {
    scope.narrow(ns, Some(Role::Admin)).is_some()
        && (!scope.read_only || matches!(action, Act::Read | Act::Export | Act::Algorithms))
}

tokio::task_local! {
    /// The caller of the request being served.
    pub static CALLER: Caller;
    /// Where the request being served comes from (14.5).
    pub static ADDRESS: Option<String>;
}

/// The address of the current request's client, when known.
pub fn address() -> Option<String> {
    ADDRESS.try_with(Clone::clone).ok().flatten()
}

/// The caller of the current request (the owner outside of one: tests,
/// tools).
pub fn current() -> Caller {
    CALLER.try_with(Clone::clone).unwrap_or(Caller::Owner)
}

/// What a request under `/v1` needs, from its method and path.
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// Anybody, signed in or not (signing in).
    Public,
    /// Any caller the server accepts.
    Caller,
    /// The owner (keys, import).
    Owner,
    /// A privilege on the server.
    Server(Act),
    /// Privileges on a namespace; the handler may ask for more (writing
    /// queries, writing algorithms).
    Namespace(String, &'static [Act]),
}

/// A percent-encoded path segment, decoded.
pub fn decode_segment(segment: &str) -> String {
    decode(segment)
}

fn decode(segment: &str) -> String {
    let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What `method` on `path` (relative to `/v1`) needs.
pub fn need(method: &str, path: &str) -> Need {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        // Signing in, refreshing, and the first account of an open server.
        ["auth", "signin"] | ["auth", "refresh"] | ["auth", "setup"] | ["auth", "status"] => {
            Need::Public
        }
        // What the engine is: its version, API version and capabilities.
        ["about"] => Need::Public,
        // The settings it runs with (no secrets, but its limits and paths).
        ["settings"] => Need::Owner,
        // Their own sessions, password and keys; the handlers check more.
        ["auth", ..] | ["keys", ..] => Need::Caller,
        ["accounts", ..] => Need::Server(Act::ManageAccounts),
        ["roles", ..] | ["admin", "roles", ..] => Need::Server(Act::ManageRoles),
        ["admin", "users", ..] => Need::Server(Act::ManageAccounts),
        // Grants check what they touch: roles, or an account's roles.
        ["admin", "grants", ..] => Need::Caller,
        ["algorithms"] | ["whoami"] | ["privileges"] => Need::Caller,
        ["metrics"] => Need::Server(Act::Metrics),
        ["audit"] => Need::Server(Act::Audit),
        // Members hand audit events to the leader with the cluster secret.
        ["cluster", "audit"] => Need::Public,
        // Listing filters what the caller may read; creating checks the
        // name in the handler.
        ["namespaces"] => Need::Caller,
        ["namespaces", ns] => Need::Namespace(
            decode(ns),
            match method {
                "GET" => &[Act::Read],
                "DELETE" => &[Act::Drop],
                _ => &[Act::Schema],
            },
        ),
        // Export checks each namespace it writes out.
        ["export"] => Need::Caller,
        ["backup"] => Need::Server(Act::Backup),
        ["cluster", ..] => Need::Server(Act::Cluster),
        ["import"] | ["background"] => Need::Owner,
        [ns, rest @ ..] => Need::Namespace(decode(ns), namespace_actions(method, rest)),
        [] => Need::Caller,
    }
}

/// What `method` on a route under a namespace needs (`rest` is the path
/// after the namespace).
fn namespace_actions(method: &str, rest: &[&str]) -> &'static [Act] {
    const READ: &[Act] = &[Act::Read];
    const WRITE: &[Act] = &[Act::Create, Act::Update, Act::Delete];
    let first = rest.first().copied().unwrap_or("");
    // Changing how the namespace works; a webhook also makes the server
    // send requests.
    let schema = matches!(first, "rebuild" | "compact" | "webhooks" | "indexes")
        || (matches!(first, "match-rules" | "constraints") && method != "GET");
    if schema {
        return &[Act::Schema];
    }
    if first.starts_with("export") {
        return &[Act::Export];
    }
    // A subscription's events carry whole records as they change (16.10).
    if first == "events" {
        return &[Act::Read, Act::Events];
    }
    if method == "GET" {
        return READ;
    }
    match (method, rest) {
        // A GQ statement: the handler checks what it does (a read, a write,
        // the schema, or managing access, which needs no namespace). So do
        // the statements of an explicit transaction (16.4), and beginning
        // or ending one needs only to read here.
        ("POST", ["query"]) => &[],
        ("POST", ["transactions", _, "query"]) => &[],
        ("POST", ["transactions"] | ["transactions", _, "commit" | "rollback"]) => READ,
        (_, ["import", ..]) => &[Act::Import],
        ("POST", ["algo", ..]) => &[Act::Algorithms],
        (
            "POST",
            ["k-hop"]
            | ["read-many"]
            | ["path"]
            | ["subgraph"]
            | ["neighborhoods"]
            | ["search", ..]
            | ["recall"]
            | ["context"]
            | ["query", "part"]
            | ["match"],
        ) => READ,
        (_, ["merge"] | ["merges", _, "undo"]) => &[Act::Merge],
        (_, ["edges", _, "retract" | "invalidate"] | ["nodes", _, "retract" | "invalidate"]) => {
            &[Act::Retract]
        }
        ("POST", ["nodes"] | ["edges"] | ["ingest"] | ["extract"] | ["nodes", _, "mentions"]) => {
            &[Act::Create]
        }
        ("DELETE", _) => &[Act::Delete],
        ("PATCH" | "PUT", _) => &[Act::Update],
        (
            "POST",
            ["touch"] | ["archive"] | ["summaries"] | ["communities", "refresh"] | ["review", _],
        ) => &[Act::Update],
        _ => WRITE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_need_roles() {
        let ns = |name: &str, actions| Need::Namespace(name.into(), actions);
        assert_eq!(need("GET", "/vault/nodes"), ns("vault", &[Act::Read]));
        assert_eq!(need("POST", "/vault/nodes"), ns("vault", &[Act::Create]));
        assert_eq!(need("PATCH", "/vault/nodes/x"), ns("vault", &[Act::Update]));
        assert_eq!(
            need("POST", "/vault/search/text"),
            ns("vault", &[Act::Read])
        );
        assert_eq!(need("POST", "/vault/query"), ns("vault", &[]));
        assert_eq!(need("POST", "/vault/merge"), ns("vault", &[Act::Merge]));
        assert_eq!(need("POST", "/vault/match"), ns("vault", &[Act::Read]));
        assert_eq!(need("POST", "/vault/read-many"), ns("vault", &[Act::Read]));
        assert_eq!(
            need("POST", "/vault/neighborhoods"),
            ns("vault", &[Act::Read])
        );
        assert_eq!(
            need("POST", "/vault/algo/pagerank"),
            ns("vault", &[Act::Algorithms])
        );
        assert_eq!(need("GET", "/vault/match-rules"), ns("vault", &[Act::Read]));
        assert_eq!(
            need("PUT", "/vault/match-rules/Person"),
            ns("vault", &[Act::Schema])
        );
        assert_eq!(need("POST", "/vault/rebuild"), ns("vault", &[Act::Schema]));
        assert_eq!(need("GET", "/vault/webhooks"), ns("vault", &[Act::Schema]));
        assert_eq!(
            need("DELETE", "/vault/edges/x"),
            ns("vault", &[Act::Delete])
        );
        assert_eq!(
            need("POST", "/vault/edges/x/retract"),
            ns("vault", &[Act::Retract])
        );
        assert_eq!(
            need("GET", "/vault/export.graphml"),
            ns("vault", &[Act::Export])
        );
        assert_eq!(
            need("POST", "/vault/import/graphml"),
            ns("vault", &[Act::Import])
        );
        assert_eq!(
            need("POST", "/vault/batch"),
            ns("vault", &[Act::Create, Act::Update, Act::Delete])
        );
        assert_eq!(
            need("DELETE", "/namespaces/team%2Da"),
            ns("team-a", &[Act::Drop])
        );
        assert_eq!(need("GET", "/namespaces/demo"), ns("demo", &[Act::Read]));
        assert_eq!(need("POST", "/import"), Need::Owner);
        assert_eq!(need("DELETE", "/keys/ana"), Need::Caller);
        assert_eq!(need("POST", "/accounts"), Need::Server(Act::ManageAccounts));
        assert_eq!(need("PUT", "/roles/x"), Need::Server(Act::ManageRoles));
        assert_eq!(
            need("PUT", "/admin/roles/x"),
            Need::Server(Act::ManageRoles)
        );
        assert_eq!(
            need("GET", "/admin/users"),
            Need::Server(Act::ManageAccounts)
        );
        assert_eq!(need("POST", "/admin/grants"), Need::Caller);
        assert_eq!(need("GET", "/metrics"), Need::Server(Act::Metrics));
        assert_eq!(need("POST", "/backup"), Need::Server(Act::Backup));
        assert_eq!(need("GET", "/cluster/status"), Need::Server(Act::Cluster));
        assert_eq!(need("POST", "/auth/signin"), Need::Public);
        assert_eq!(need("POST", "/auth/signout"), Need::Caller);
        assert_eq!(need("GET", "/namespaces"), Need::Caller);
    }

    #[test]
    fn passwords_and_tokens() {
        let hash = hash_password("a long passphrase");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "a long passphrase"));
        assert!(!verify_password(&hash, "a long passphrasf"));
        assert!(!verify_password("not a hash", "a long passphrase"));
        assert!(check_password("ana", "short").is_err());
        assert!(check_password("ana", "password123").is_err());
        assert!(check_password("anabelle", "my anabelle 2024").is_err());
        assert!(check_password("ana", "aaaaaaaaaaaa").is_err());
        assert!(check_password("ana", "correct horse battery").is_ok());
        let (token, secret) = new_token(ACCESS_PREFIX, "abc");
        assert_eq!(
            split_token(&token, ACCESS_PREFIX),
            Some(("abc", secret.as_str()))
        );
        assert_eq!(split_token(&token, REFRESH_PREFIX), None);
    }

    #[test]
    fn secrets_are_hashed() {
        let secret = new_secret();
        assert!(secret.starts_with("gfk_") && secret.len() == 44);
        assert_eq!(hash_secret(&secret).len(), 64);
        assert_ne!(hash_secret(&secret), hash_secret(&new_secret()));
    }
}
