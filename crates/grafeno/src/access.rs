//! Accounts, their API keys and their sessions (phase 14.1), stored in the
//! database (only hashes of secrets and passwords). Servers check them;
//! the engine records who wrote what ([`crate::Actor`]).
//!
//! An account is a *user* (a person: a password, sessions) or a *service
//! account* (an agent or an application: API keys only). Both hold grants
//! per namespace (read, write, admin), may be superusers, and may be
//! limited to changing what they recorded themselves. A key belongs to an
//! account, may expire, and may be scoped down to some namespaces or to
//! reading. A session is what signing in with a password gives: a
//! short-lived access token and a refresh token, both stored as hashes.
//! In a cluster all of it is replicated state, written through the log.
//!
//! Layout 15 replaced the principals of 10.1 (one named key with grants)
//! with these: each principal became a service account with its grants and
//! a key of the same name and secret, so `recorded_by` keeps its names.

use grafeno_core::{Error, Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::store::{decode, encode, storage};

/// `name -> Principal` (JSON), up to layout 14.
pub(crate) const PRINCIPALS: TableDefinition<&str, &[u8]> = TableDefinition::new("principals");
/// `name -> Account` (JSON).
pub(crate) const ACCOUNTS: TableDefinition<&str, &[u8]> = TableDefinition::new("accounts");
/// `name -> ApiKey` (JSON).
pub(crate) const API_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("api_keys");
/// `id -> Session` (JSON).
pub(crate) const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");

/// What a grant allows, each role including the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Read and search.
    Read,
    /// Also write nodes and facts, merge and archive.
    Write,
    /// Also indexes, rebuilds, imports, and creating, describing and
    /// dropping the namespace.
    Admin,
}

impl Role {
    pub fn parse(text: &str) -> Result<Self> {
        match text.to_ascii_lowercase().as_str() {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            other => Err(Error::Invalid(format!(
                "unknown role {other:?} (read, write, admin)"
            ))),
        }
    }
}

/// A role on the namespaces a pattern names: `vault` (that one), `team-*`
/// (every name starting with `team-`) or `*` (all).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub namespace: String,
    pub role: Role,
}

impl Grant {
    pub fn covers(&self, ns: &str) -> bool {
        covers(&self.namespace, ns)
    }
}

fn covers(pattern: &str, ns: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => ns.starts_with(prefix),
        None => pattern == ns,
    }
}

/// A person or a program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    /// Signs in with a password; may also hold keys.
    User,
    /// API keys only.
    #[default]
    Service,
}

/// Who may do what.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub kind: AccountKind,
    /// An Argon2id hash in PHC format (users only); the password itself is
    /// never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Roles granted to it (14.2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// Every right on every namespace, and managing accounts.
    #[serde(default)]
    pub superuser: bool,
    /// May change only nodes and facts it recorded itself.
    #[serde(default)]
    pub own_only: bool,
    /// A disabled account can neither sign in nor use its keys.
    #[serde(default)]
    pub disabled: bool,
    /// What record policies compare properties with (14.4): `tenant`,
    /// `team`.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, serde_json::Value>,
    /// After this time the account can neither sign in nor use its keys
    /// (14.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: Timestamp,
}

impl Account {
    /// Whether it has expired at `now`.
    pub fn expired(&self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }

    /// The highest role it has on `ns`.
    pub fn role_in(&self, ns: &str) -> Option<Role> {
        if self.superuser {
            return Some(Role::Admin);
        }
        self.grants
            .iter()
            .filter(|grant| grant.covers(ns))
            .map(|grant| grant.role)
            .max()
    }

    pub fn may(&self, ns: &str, role: Role) -> bool {
        self.role_in(ns).is_some_and(|have| have >= role)
    }
}

/// What a key is narrowed to, inside its account's rights.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyScope {
    /// Namespace patterns it may reach (all of the account's when empty).
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// Read only, whatever the account may do.
    #[serde(default)]
    pub read_only: bool,
}

impl KeyScope {
    /// The role `role` on `ns` once the scope applies, from what the
    /// account has.
    pub fn narrow(&self, ns: &str, role: Option<Role>) -> Option<Role> {
        if !self.namespaces.is_empty() && !self.namespaces.iter().any(|p| covers(p, ns)) {
            return None;
        }
        match role {
            Some(_) if self.read_only => Some(Role::Read),
            other => other,
        }
    }

    /// The scope a key made by a key of scope `self` may have, given the
    /// scope asked for (`None`: no narrowing asked). A key never reaches
    /// further than the key that made it: no scope asked gives `self`, and
    /// a scope that reaches a namespace `self` does not, or writes where
    /// `self` only reads, is refused (`None`).
    pub fn narrowed(&self, wanted: Option<&KeyScope>) -> Option<KeyScope> {
        let Some(wanted) = wanted else {
            return Some(self.clone());
        };
        if self.read_only && !wanted.read_only {
            return None;
        }
        let namespaces = if self.namespaces.is_empty() {
            wanted.namespaces.clone()
        } else if wanted.namespaces.is_empty() {
            // All of the account's, which this key does not reach.
            self.namespaces.clone()
        } else {
            for pattern in &wanted.namespaces {
                let inside = self.namespaces.iter().any(|outer| {
                    outer == pattern
                        || match pattern.strip_suffix('*') {
                            // A wider pattern is inside only a pattern
                            // that starts the same way or earlier.
                            Some(prefix) => outer
                                .strip_suffix('*')
                                .is_some_and(|outer| prefix.starts_with(outer)),
                            None => covers(outer, pattern),
                        }
                });
                if !inside {
                    return None;
                }
            }
            wanted.namespaces.clone()
        };
        Some(KeyScope {
            namespaces,
            read_only: self.read_only || wanted.read_only,
        })
    }
}

/// An API key of an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    pub name: String,
    pub account: String,
    /// The hex SHA-256 of the secret; the secret itself is never stored.
    pub secret_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<KeyScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: Timestamp,
    /// When its secret was last replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<Timestamp>,
}

impl ApiKey {
    pub fn expired(&self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// A signed-in user's session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub account: String,
    /// Hex SHA-256 of the current access token's secret.
    pub token_hash: String,
    pub expires_at: Timestamp,
    /// Hex SHA-256 of the current refresh token's secret.
    pub refresh_hash: String,
    pub refresh_expires_at: Timestamp,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<Timestamp>,
    /// Where it signed in from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// A named API key with grants, up to layout 14 (now an account and a key).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub name: String,
    pub secret_hash: String,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub owner: bool,
    #[serde(default)]
    pub own_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: Timestamp,
}

impl Principal {
    /// The service account and the key it becomes.
    pub fn split(&self) -> (Account, ApiKey) {
        (
            Account {
                name: self.name.clone(),
                kind: AccountKind::Service,
                password_hash: None,
                grants: self.grants.clone(),
                roles: Vec::new(),
                superuser: self.owner,
                own_only: self.own_only,
                disabled: false,
                expires_at: None,
                attributes: Default::default(),
                description: self.description.clone(),
                created_at: self.created_at,
            },
            ApiKey {
                name: self.name.clone(),
                account: self.name.clone(),
                secret_hash: self.secret_hash.clone(),
                scope: None,
                expires_at: None,
                description: None,
                created_at: self.created_at,
                rotated_at: None,
            },
        )
    }
}

/// Checks an account's or a key's name: it ends up in provenance and in
/// URLs.
pub fn check_principal_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
    if valid {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "a name has 1 to 64 letters, digits, '-', '_', '.' or '@', not {name:?}"
        )))
    }
}

fn all<T: serde::de::DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for item in table.iter().map_err(storage)? {
        let (_, value) = item.map_err(storage)?;
        out.push(decode(value.value())?);
    }
    Ok(out)
}

fn one<T: serde::de::DeserializeOwned>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<T>> {
    table
        .get(key)
        .map_err(storage)?
        .map(|guard| decode(guard.value()))
        .transpose()
}

impl ReadTx {
    /// Every account, in name order.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        all(&self.tx.open_table(ACCOUNTS).map_err(storage)?)
    }

    pub fn account(&self, name: &str) -> Result<Option<Account>> {
        one(&self.tx.open_table(ACCOUNTS).map_err(storage)?, name)
    }

    /// Every API key, in name order.
    pub fn api_keys(&self) -> Result<Vec<ApiKey>> {
        all(&self.tx.open_table(API_KEYS).map_err(storage)?)
    }

    pub fn api_key(&self, name: &str) -> Result<Option<ApiKey>> {
        one(&self.tx.open_table(API_KEYS).map_err(storage)?, name)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        one(&self.tx.open_table(SESSIONS).map_err(storage)?, id)
    }

    /// The sessions of `account` (all of them with `None`), in id order.
    pub fn sessions(&self, account: Option<&str>) -> Result<Vec<Session>> {
        let sessions: Vec<Session> = all(&self.tx.open_table(SESSIONS).map_err(storage)?)?;
        Ok(sessions
            .into_iter()
            .filter(|s| account.is_none_or(|name| s.account == name))
            .collect())
    }
}

impl WriteTx {
    pub fn accounts(&self) -> Result<Vec<Account>> {
        all(&self.tx.open_table(ACCOUNTS).map_err(storage)?)
    }

    pub fn account(&self, name: &str) -> Result<Option<Account>> {
        one(&self.tx.open_table(ACCOUNTS).map_err(storage)?, name)
    }

    pub fn api_key(&self, name: &str) -> Result<Option<ApiKey>> {
        one(&self.tx.open_table(API_KEYS).map_err(storage)?, name)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        one(&self.tx.open_table(SESSIONS).map_err(storage)?, id)
    }

    /// Every API key, inside this write.
    pub fn all_api_keys(&self) -> Result<Vec<ApiKey>> {
        all(&self.tx.open_table(API_KEYS).map_err(storage)?)
    }

    /// Every session, inside this write.
    pub fn all_sessions(&self) -> Result<Vec<Session>> {
        all(&self.tx.open_table(SESSIONS).map_err(storage)?)
    }

    /// Adds an account. Fails with [`Error::Conflict`] if the name is taken.
    pub fn create_account(&mut self, account: &Account) -> Result<()> {
        check_principal_name(&account.name)?;
        if self.account(&account.name)?.is_some() {
            return Err(Error::Conflict(format!(
                "an account named {:?} exists",
                account.name
            )));
        }
        self.put_account(account)
    }

    /// Writes an account as it is (created or changed).
    pub fn put_account(&mut self, account: &Account) -> Result<()> {
        check_principal_name(&account.name)?;
        for role in &account.roles {
            if self.role(role)?.is_none() {
                return Err(Error::Invalid(format!("no role {role:?}")));
            }
        }
        let before = self.account(&account.name)?;
        self.note(crate::replication::Touch::Account(account.name.clone()));
        self.tx
            .open_table(ACCOUNTS)
            .map_err(storage)?
            .insert(account.name.as_str(), encode(account)?.as_slice())
            .map_err(storage)?;
        self.bump_access()?;
        let (kind, detail) = match &before {
            None => ("account_created", summary(account)),
            Some(before) => ("account_changed", changes(before, account)),
        };
        self.audit_access(kind, &account.name, detail)
    }

    /// Removes an account with its keys and sessions; returns whether it
    /// existed. What it recorded keeps its name.
    pub fn delete_account(&mut self, name: &str) -> Result<bool> {
        let keys: Vec<ApiKey> = all(&self.tx.open_table(API_KEYS).map_err(storage)?)?;
        for key in keys.iter().filter(|k| k.account == name) {
            self.delete_api_key(&key.name)?;
        }
        let sessions: Vec<Session> = all(&self.tx.open_table(SESSIONS).map_err(storage)?)?;
        for session in sessions.iter().filter(|s| s.account == name) {
            self.delete_session(&session.id)?;
        }
        self.note(crate::replication::Touch::Account(name.to_owned()));
        let removed = self
            .tx
            .open_table(ACCOUNTS)
            .map_err(storage)?
            .remove(name)
            .map_err(storage)?
            .is_some();
        self.bump_access()?;
        if removed {
            self.audit_access("account_deleted", name, serde_json::Value::Null)?;
        }
        Ok(removed)
    }

    /// Adds a key to an existing account. Fails with [`Error::Conflict`] if
    /// the name is taken.
    pub fn create_api_key(&mut self, key: &ApiKey) -> Result<()> {
        check_principal_name(&key.name)?;
        if self.account(&key.account)?.is_none() {
            return Err(Error::Invalid(format!(
                "there is no account {:?} to hold the key",
                key.account
            )));
        }
        if self.api_key(&key.name)?.is_some() {
            return Err(Error::Conflict(format!(
                "a key named {:?} exists",
                key.name
            )));
        }
        self.put_api_key(key)
    }

    /// Writes a key as it is (a new secret, a new expiry).
    pub fn put_api_key(&mut self, key: &ApiKey) -> Result<()> {
        let before = self.api_key(&key.name)?;
        self.note(crate::replication::Touch::ApiKey(key.name.clone()));
        self.tx
            .open_table(API_KEYS)
            .map_err(storage)?
            .insert(key.name.as_str(), encode(key)?.as_slice())
            .map_err(storage)?;
        let kind = match &before {
            None => "key_created",
            Some(before) if before.secret_hash != key.secret_hash => "key_rotated",
            Some(_) => "key_changed",
        };
        self.audit_access(
            kind,
            &key.name,
            serde_json::json!({ "account": key.account, "scope": key.scope, "expires_at": key.expires_at }),
        )
    }

    /// Removes a key; its secret stops working. Returns whether it existed.
    pub fn delete_api_key(&mut self, name: &str) -> Result<bool> {
        let before = self.api_key(name)?;
        self.note(crate::replication::Touch::ApiKey(name.to_owned()));
        let removed = self
            .tx
            .open_table(API_KEYS)
            .map_err(storage)?
            .remove(name)
            .map_err(storage)?
            .is_some();
        if let Some(key) = before {
            self.audit_access(
                "key_revoked",
                name,
                serde_json::json!({ "account": key.account }),
            )?;
        }
        Ok(removed)
    }

    /// Writes a session as it is (signed in, refreshed).
    pub fn put_session(&mut self, session: &Session) -> Result<()> {
        let existed = self.session(&session.id)?.is_some();
        self.note(crate::replication::Touch::Session(session.id.clone()));
        self.tx
            .open_table(SESSIONS)
            .map_err(storage)?
            .insert(session.id.as_str(), encode(session)?.as_slice())
            .map_err(storage)?;
        self.audit_access(
            if existed {
                "session_refreshed"
            } else {
                "session_created"
            },
            &session.account,
            serde_json::json!({ "session": session.id, "address": session.address }),
        )
    }

    /// Ends a session; returns whether it existed.
    pub fn delete_session(&mut self, id: &str) -> Result<bool> {
        let before = self.session(id)?;
        self.note(crate::replication::Touch::Session(id.to_owned()));
        let removed = self
            .tx
            .open_table(SESSIONS)
            .map_err(storage)?
            .remove(id)
            .map_err(storage)?
            .is_some();
        if let Some(session) = before {
            self.audit_access(
                "session_ended",
                &session.account,
                serde_json::json!({ "session": id }),
            )?;
        }
        Ok(removed)
    }

    /// Ends the sessions of `account` whose refresh token expired before
    /// `now`; returns how many.
    pub fn prune_sessions(&mut self, account: &str, now: Timestamp) -> Result<usize> {
        let sessions: Vec<Session> = all(&self.tx.open_table(SESSIONS).map_err(storage)?)?;
        let expired: Vec<String> = sessions
            .into_iter()
            .filter(|s| s.account == account && s.refresh_expires_at <= now)
            .map(|s| s.id)
            .collect();
        for id in &expired {
            self.delete_session(id)?;
        }
        Ok(expired.len())
    }
}

/// What the audit log says of an account: everything but its password's
/// hash (whether it has one).
fn summary(account: &Account) -> serde_json::Value {
    serde_json::json!({
        "kind": account.kind,
        "has_password": account.password_hash.is_some(),
        "grants": account.grants,
        "roles": account.roles,
        "superuser": account.superuser,
        "own_only": account.own_only,
        "disabled": account.disabled,
        "expires_at": account.expires_at,
        "attributes": account.attributes,
    })
}

/// What changed in an account, for the audit log (a new password is named,
/// never shown).
fn changes(before: &Account, after: &Account) -> serde_json::Value {
    let (old, new) = (summary(before), summary(after));
    let mut changed = serde_json::Map::new();
    if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
        for (key, value) in new {
            if old.get(key) != Some(value) {
                changed.insert(key.clone(), value.clone());
            }
        }
    }
    if before.password_hash != after.password_hash && after.password_hash.is_some() {
        changed.insert("password".into(), serde_json::json!("changed"));
    }
    serde_json::Value::Object(changed)
}

/// Layout 15: every principal becomes a service account and a key of the
/// same name and secret.
pub(crate) fn upgrade_principals(tx: &redb::WriteTransaction) -> Result<()> {
    let principals: Vec<Principal> = match tx.open_table(PRINCIPALS) {
        Ok(table) => all(&table)?,
        Err(_) => Vec::new(),
    };
    {
        let mut accounts = tx.open_table(ACCOUNTS).map_err(storage)?;
        let mut keys = tx.open_table(API_KEYS).map_err(storage)?;
        tx.open_table(SESSIONS).map_err(storage)?;
        for principal in &principals {
            let (account, key) = principal.split();
            accounts
                .insert(account.name.as_str(), encode(&account)?.as_slice())
                .map_err(storage)?;
            keys.insert(key.name.as_str(), encode(&key)?.as_slice())
                .map_err(storage)?;
        }
    }
    let _ = tx.delete_table(PRINCIPALS);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::KeyScope;

    fn scope(namespaces: &[&str], read_only: bool) -> KeyScope {
        KeyScope {
            namespaces: namespaces.iter().map(|ns| ns.to_string()).collect(),
            read_only,
        }
    }

    #[test]
    fn a_key_made_by_a_scoped_key_reaches_no_further() {
        let outer = scope(&["public", "team-*"], true);
        // Nothing asked: the same scope, not the account's whole reach.
        assert_eq!(outer.narrowed(None), Some(outer.clone()));
        // Narrower: kept.
        assert_eq!(
            outer.narrowed(Some(&scope(&["team-a"], true))),
            Some(scope(&["team-a"], true))
        );
        assert_eq!(
            outer.narrowed(Some(&scope(&["team-a*"], true))),
            Some(scope(&["team-a*"], true))
        );
        // An empty list of namespaces means all: the outer ones.
        assert_eq!(
            outer.narrowed(Some(&scope(&[], true))),
            Some(scope(&["public", "team-*"], true))
        );
        // Wider: refused.
        assert_eq!(outer.narrowed(Some(&scope(&["secret"], true))), None);
        assert_eq!(outer.narrowed(Some(&scope(&["*"], true))), None);
        assert_eq!(outer.narrowed(Some(&scope(&["te*"], true))), None);
        assert_eq!(outer.narrowed(Some(&scope(&["public"], false))), None);
        // An outer key that writes may make one that only reads.
        let writer = scope(&["public"], false);
        assert_eq!(
            writer.narrowed(Some(&scope(&["public"], true))),
            Some(scope(&["public"], true))
        );
        let everywhere_read = scope(&[], true);
        assert_eq!(everywhere_read.narrowed(Some(&scope(&["x"], false))), None);
        assert_eq!(
            everywhere_read.narrowed(Some(&scope(&["x"], true))),
            Some(scope(&["x"], true))
        );
    }
    use super::*;

    fn account(grants: &[(&str, Role)]) -> Account {
        Account {
            name: "agent".into(),
            kind: AccountKind::Service,
            password_hash: None,
            grants: grants
                .iter()
                .map(|(namespace, role)| Grant {
                    namespace: (*namespace).into(),
                    role: *role,
                })
                .collect(),
            roles: Vec::new(),
            superuser: false,
            own_only: false,
            disabled: false,
            expires_at: None,
            attributes: Default::default(),
            description: None,
            created_at: Timestamp::from_micros(0),
        }
    }

    #[test]
    fn grants_and_roles() {
        let p = account(&[
            ("vault", Role::Write),
            ("team-*", Role::Read),
            ("team-ops", Role::Admin),
        ]);
        assert!(p.may("vault", Role::Read) && p.may("vault", Role::Write));
        assert!(!p.may("vault", Role::Admin));
        assert!(p.may("team-a", Role::Read) && !p.may("team-a", Role::Write));
        assert!(p.may("team-ops", Role::Admin), "the highest grant wins");
        assert!(
            !p.may("vault2", Role::Read),
            "exact names do not match prefixes"
        );
        assert!(!p.may("team", Role::Read));
        assert_eq!(p.role_in("other"), None);
        let all = account(&[("*", Role::Read)]);
        assert!(all.may("anything", Role::Read));
        assert!(check_principal_name("ana@agents").is_ok());
        assert!(check_principal_name("a b").is_err());
        assert!(Role::parse("WRITE").is_ok() && Role::parse("root").is_err());
    }

    #[test]
    fn scopes_narrow_keys() {
        let scope = KeyScope {
            namespaces: vec!["team-*".into()],
            read_only: true,
        };
        assert_eq!(scope.narrow("team-a", Some(Role::Admin)), Some(Role::Read));
        assert_eq!(scope.narrow("vault", Some(Role::Admin)), None);
        assert_eq!(scope.narrow("team-a", None), None);
        let open = KeyScope::default();
        assert_eq!(open.narrow("vault", Some(Role::Write)), Some(Role::Write));
    }
}
