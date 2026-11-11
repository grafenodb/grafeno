//! Roles and privileges (phase 14.2).
//!
//! A privilege is an action on an object, granted or denied:
//!
//! - on the server: create namespaces, manage accounts, manage roles and
//!   grants, back up and restore, see metrics, see the audit log, operate
//!   the cluster;
//! - on a namespace (a name, a `prefix-*` or `*`): read (match, search,
//!   recall), create, update, delete, merge, retract, import, export,
//!   manage the schema (indexes, constraints, match rules), run
//!   algorithms, drop, and create namespaces whose names match;
//! - on a label inside a namespace (of nodes, or of edges): read or write;
//! - on a property of a label: read or write.
//!
//! A role holds privileges, the built-in levels on namespace patterns
//! (`reader`, `editor`, `admin`, as the grants of 10.1 were) and other
//! roles (a hierarchy; cycles are refused). An account's rights are the
//! union of its grants, its roles (followed down the hierarchy), and the
//! role `public`, which every account has when it is defined; a superuser
//! has every right. A denial wins over any grant at any level, so "read
//! everything in `crm` except `Salary`" is one grant and one denial.
//!
//! A role can also hold record policies (14.4): limits to the records that
//! match a condition (`@recorded_by` is the caller, `tenant` is the
//! caller's `tenant` attribute), for reads, writes or both. Policies narrow,
//! like denials: a record must match every policy that applies to it.
//!
//! [`Rights`] is what an account may do, computed once (from the account
//! and every role) and kept until a role or the account changes
//! ([`ReadTx::access_generation`]), so that a check is a lookup.

use std::collections::{HashMap, HashSet};

use grafeno_core::{Error, Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::access::{Account, Grant, Role};
use crate::db::{ReadTx, WriteTx};
use crate::store::{META, decode, encode, storage};

/// `name -> RoleDef` (JSON).
pub(crate) const ROLES: TableDefinition<&str, &[u8]> = TableDefinition::new("roles");
/// The META key counting changes to accounts and roles.
pub(crate) const ACCESS_GENERATION: &str = "access_generation";

/// Names a role cannot take: the built-in levels and the superuser.
pub const BUILT_IN_ROLES: [&str; 4] = ["reader", "editor", "admin", "superuser"];
/// The role every account has, when it is defined.
pub const PUBLIC: &str = "public";

/// What a privilege allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Act {
    // On a namespace.
    Read,
    Create,
    Update,
    Delete,
    Merge,
    Retract,
    Import,
    Export,
    /// Indexes, constraints, match rules, rebuilds, webhooks.
    Schema,
    Algorithms,
    Drop,
    /// Create namespaces (on the server: any; on a pattern: matching ones).
    CreateNamespace,
    // On a label or a property: creating, changing and deleting it.
    Write,
    // On the server.
    ManageAccounts,
    ManageRoles,
    Backup,
    Metrics,
    Audit,
    Cluster,
    /// Event subscriptions on a namespace and their events (16.10).
    Events,
}

impl Act {
    /// Every action on a namespace, in order.
    pub const NAMESPACE: [Act; 13] = [
        Act::Read,
        Act::Create,
        Act::Update,
        Act::Delete,
        Act::Merge,
        Act::Retract,
        Act::Import,
        Act::Export,
        Act::Schema,
        Act::Algorithms,
        Act::Drop,
        Act::CreateNamespace,
        Act::Events,
    ];
    /// Every action on the server.
    pub const SERVER: [Act; 7] = [
        Act::CreateNamespace,
        Act::ManageAccounts,
        Act::ManageRoles,
        Act::Backup,
        Act::Metrics,
        Act::Audit,
        Act::Cluster,
    ];

    fn bit(self) -> u32 {
        1 << (self as u32)
    }

    pub fn parse(text: &str) -> Result<Self> {
        serde_json::from_value(serde_json::Value::String(text.to_ascii_lowercase()))
            .map_err(|_| Error::Invalid(format!("unknown privilege {text:?}")))
    }

    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

/// What a privilege is on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum Object {
    Server,
    Namespace {
        namespace: String,
    },
    /// Nodes with a label, or edges with it.
    Label {
        namespace: String,
        label: String,
        #[serde(default)]
        edges: bool,
    },
    Property {
        namespace: String,
        label: String,
        property: String,
    },
}

/// An action on an object, granted or denied.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Privilege {
    pub action: Act,
    pub object: Object,
    /// A denial wins over any grant.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deny: bool,
}

impl Privilege {
    pub fn grant(action: Act, object: Object) -> Self {
        Self {
            action,
            object,
            deny: false,
        }
    }

    pub fn deny(action: Act, object: Object) -> Self {
        Self {
            action,
            object,
            deny: true,
        }
    }

    /// Whether the action makes sense on the object.
    pub fn check(&self) -> Result<()> {
        let ok = match &self.object {
            Object::Server => Act::SERVER.contains(&self.action),
            Object::Namespace { .. } => Act::NAMESPACE.contains(&self.action),
            Object::Label { .. } | Object::Property { .. } => {
                matches!(self.action, Act::Read | Act::Write)
            }
        };
        if ok {
            Ok(())
        } else {
            Err(Error::Invalid(format!(
                "the privilege {} does not apply to {:?}",
                self.action.name(),
                self.object
            )))
        }
    }
}

/// A role an administrator defined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDef {
    pub name: String,
    #[serde(default)]
    pub privileges: Vec<Privilege>,
    /// Built-in levels on namespace patterns (`reader`, `editor`, `admin`).
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Roles it has (their rights are its own).
    #[serde(default)]
    pub roles: Vec<String>,
    /// Record policies (14.4): who holds the role sees and changes only
    /// the records that match them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<Policy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: Timestamp,
}

/// A limit to the records that match a condition, in the namespaces of a
/// pattern, on the nodes with a label (or on every node and edge).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Policy {
    /// A namespace pattern: a name, `prefix-*` or `*`.
    pub namespace: String,
    /// Nodes with this label only; every node and edge when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Reads, writes or both.
    #[serde(default)]
    pub applies: Applies,
    pub condition: Condition,
}

/// What a policy limits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    Read,
    Write,
    #[default]
    All,
}

/// What a record must match.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    /// It was recorded by the caller (`@recorded_by = current_user()`).
    RecordedByCaller,
    /// A property equals a value.
    Property { key: String, equals: PolicyValue },
}

/// The value a property is compared with.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum PolicyValue {
    /// The caller's account name.
    Caller,
    /// One of the caller's attributes (`$account.tenant`).
    Attribute { name: String },
    /// A JSON value written in the policy.
    Value { value: serde_json::Value },
}

impl std::fmt::Display for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let what = match &self.condition {
            Condition::RecordedByCaller => "@recorded_by = current_user()".to_owned(),
            Condition::Property { key, equals } => format!(
                "{key} = {}",
                match equals {
                    PolicyValue::Caller => "current_user()".to_owned(),
                    PolicyValue::Attribute { name } => format!("$account.{name}"),
                    PolicyValue::Value { value } => value.to_string(),
                }
            ),
        };
        write!(
            f,
            "{} in {}{}: {what}",
            match self.applies {
                Applies::Read => "reads",
                Applies::Write => "writes",
                Applies::All => "reads and writes",
            },
            self.namespace,
            self.label
                .as_ref()
                .map(|label| format!(", nodes {label}"))
                .unwrap_or_default()
        )
    }
}

/// What a built-in level allows on a namespace.
pub fn level_actions(role: Role) -> &'static [Act] {
    match role {
        Role::Read => &[Act::Read, Act::Export, Act::Algorithms],
        Role::Write => &[
            Act::Read,
            Act::Export,
            Act::Algorithms,
            Act::Create,
            Act::Update,
            Act::Delete,
            Act::Merge,
            Act::Retract,
        ],
        Role::Admin => &[
            Act::Read,
            Act::Export,
            Act::Algorithms,
            Act::Create,
            Act::Update,
            Act::Delete,
            Act::Merge,
            Act::Retract,
            Act::Import,
            Act::Schema,
            Act::Drop,
            Act::CreateNamespace,
            Act::Events,
        ],
    }
}

/// The built-in name of a level.
pub fn level_name(role: Role) -> &'static str {
    match role {
        Role::Read => "reader",
        Role::Write => "editor",
        Role::Admin => "admin",
    }
}

/// Granted and denied actions, as bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bits {
    allow: u32,
    deny: u32,
}

impl Bits {
    fn add(&mut self, action: Act, deny: bool) {
        if deny {
            self.deny |= action.bit();
        } else {
            self.allow |= action.bit();
        }
    }

    fn merge(&mut self, other: Bits) {
        self.allow |= other.allow;
        self.deny |= other.deny;
    }
}

/// Namespace patterns with their bits: exact names in a map, prefixes in
/// a list, `*` apart.
#[derive(Clone, Debug, Default)]
struct Patterns<K: Eq + std::hash::Hash> {
    exact: HashMap<(String, K), Bits>,
    prefixes: Vec<(String, K, Bits)>,
}

impl<K: Eq + std::hash::Hash + Clone> Patterns<K> {
    fn add(&mut self, pattern: &str, key: K, action: Act, deny: bool) {
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                match self
                    .prefixes
                    .iter_mut()
                    .find(|(p, k, _)| p == prefix && *k == key)
                {
                    Some((_, _, bits)) => bits.add(action, deny),
                    None => {
                        let mut bits = Bits::default();
                        bits.add(action, deny);
                        self.prefixes.push((prefix.to_owned(), key, bits));
                    }
                }
            }
            None => self
                .exact
                .entry((pattern.to_owned(), key))
                .or_default()
                .add(action, deny),
        }
    }

    /// The bits of every pattern that matches `ns`, for `key`.
    fn bits(&self, ns: &str, key: &K) -> Bits {
        let mut out = Bits::default();
        if let Some(bits) = self.exact.get(&(ns.to_owned(), key.clone())) {
            out.merge(*bits);
        }
        for (prefix, k, bits) in &self.prefixes {
            if k == key && ns.starts_with(prefix.as_str()) {
                out.merge(*bits);
            }
        }
        out
    }
}

/// What an account may do, computed once.
#[derive(Clone, Debug, Default)]
pub struct Rights {
    pub superuser: bool,
    server: Bits,
    namespaces: Patterns<()>,
    /// Keyed by `(label, edges)`.
    labels: Patterns<(String, bool)>,
    /// Keyed by `(label, property)`.
    properties: Patterns<(String, String)>,
    /// The roles followed, in the order met (for display).
    pub roles: Vec<String>,
    /// The record policies of every role followed (14.4).
    pub policies: Vec<Policy>,
}

impl Rights {
    /// The rights of `account` given every defined role.
    pub fn compute(account: &Account, roles: &HashMap<String, RoleDef>) -> Self {
        let mut rights = Rights {
            superuser: account.superuser,
            ..Rights::default()
        };
        let mut seen = HashSet::new();
        let mut pending: Vec<String> = account.roles.clone();
        if roles.contains_key(PUBLIC) {
            pending.push(PUBLIC.to_owned());
        }
        for grant in &account.grants {
            rights.add_grant(grant);
        }
        while let Some(name) = pending.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(role) = roles.get(&name) else {
                continue;
            };
            rights.roles.push(name);
            for grant in &role.grants {
                rights.add_grant(grant);
            }
            for privilege in &role.privileges {
                rights.add(privilege);
            }
            for policy in &role.policies {
                if !rights.policies.contains(policy) {
                    rights.policies.push(policy.clone());
                }
            }
            pending.extend(role.roles.iter().cloned());
        }
        rights
    }

    fn add_grant(&mut self, grant: &Grant) {
        for action in level_actions(grant.role) {
            self.namespaces.add(&grant.namespace, (), *action, false);
        }
    }

    fn add(&mut self, privilege: &Privilege) {
        let (action, deny) = (privilege.action, privilege.deny);
        match &privilege.object {
            Object::Server => self.server.add(action, deny),
            Object::Namespace { namespace } => self.namespaces.add(namespace, (), action, deny),
            Object::Label {
                namespace,
                label,
                edges,
            } => self
                .labels
                .add(namespace, (label.clone(), *edges), action, deny),
            Object::Property {
                namespace,
                label,
                property,
            } => self
                .properties
                .add(namespace, (label.clone(), property.clone()), action, deny),
        }
    }

    /// Whether reading in `ns` depends on the record: label or property
    /// rules or record policies apply there. Without them, a namespace's
    /// records are all visible or all hidden, and nothing is checked per
    /// record.
    pub fn per_record(&self, ns: &str) -> bool {
        if self.superuser {
            return false;
        }
        let matches = |pattern: &str| match pattern.strip_suffix('*') {
            Some(prefix) => ns.starts_with(prefix),
            None => pattern == ns,
        };
        self.labels.exact.keys().any(|(p, _)| p == ns)
            || self
                .labels
                .prefixes
                .iter()
                .any(|(p, _, _)| ns.starts_with(p.as_str()))
            || self.properties.exact.keys().any(|(p, _)| p == ns)
            || self
                .properties
                .prefixes
                .iter()
                .any(|(p, _, _)| ns.starts_with(p.as_str()))
            || self
                .policies
                .iter()
                .any(|policy| matches(&policy.namespace))
    }

    /// Whether some label or property of `ns` may be written: the route may
    /// let a write through and leave each record to [`Rights::on_label`].
    pub fn writes_labels_in(&self, ns: &str) -> bool {
        let write = |bits: &Bits| bits.allow & Act::Write.bit() != 0;
        self.labels
            .exact
            .iter()
            .any(|((p, _), bits)| p == ns && write(bits))
            || self
                .labels
                .prefixes
                .iter()
                .any(|(p, _, bits)| ns.starts_with(p.as_str()) && write(bits))
            || self
                .properties
                .exact
                .iter()
                .any(|((p, _), bits)| p == ns && write(bits))
            || self
                .properties
                .prefixes
                .iter()
                .any(|(p, _, bits)| ns.starts_with(p.as_str()) && write(bits))
    }

    /// May the account do `action` on a record with these labels (nodes),
    /// or this label (`edges`)? Every label must be allowed it at the
    /// namespace or label level, or one label granted it with no other
    /// denying it: a denial on any label wins.
    pub fn on_labels(&self, action: Act, ns: &str, labels: &[String], edges: bool) -> bool {
        if self.superuser {
            return true;
        }
        let mut allowed = false;
        for label in labels {
            let (allow, deny) = self.label_bits(action, ns, label, edges);
            if deny {
                return false;
            }
            allowed |= allow;
        }
        allowed || (labels.is_empty() && self.on_namespace(action, ns))
    }

    /// Is `property` hidden (for `Act::Read`) or not writable (any other
    /// action) on a record with these labels? A denial on any label's
    /// property wins.
    pub fn property_denied(
        &self,
        action: Act,
        ns: &str,
        labels: &[String],
        property: &str,
    ) -> bool {
        if self.superuser {
            return false;
        }
        let level = if action == Act::Read {
            Act::Read
        } else {
            Act::Write
        };
        labels.iter().any(|label| {
            self.properties
                .bits(ns, &(label.clone(), property.to_owned()))
                .deny
                & level.bit()
                != 0
        })
    }

    /// May the account do `action` on the server?
    pub fn on_server(&self, action: Act) -> bool {
        self.superuser
            || (self.server.allow & action.bit() != 0 && self.server.deny & action.bit() == 0)
    }

    /// May the account do `action` in namespace `ns`? (`CreateNamespace`
    /// also passes with the server-wide privilege.)
    pub fn on_namespace(&self, action: Act, ns: &str) -> bool {
        if self.superuser {
            return true;
        }
        let bits = self.namespaces.bits(ns, &());
        if bits.deny & action.bit() != 0 {
            return false;
        }
        bits.allow & action.bit() != 0
            || (action == Act::CreateNamespace && self.on_server(Act::CreateNamespace))
    }

    /// The namespace action a label or property privilege stands for.
    fn label_bits(&self, action: Act, ns: &str, label: &str, edges: bool) -> (bool, bool) {
        let level = if action == Act::Read {
            Act::Read
        } else {
            Act::Write
        };
        let ns_bits = self.namespaces.bits(ns, &());
        let label_bits = self.labels.bits(ns, &(label.to_owned(), edges));
        let allow = ns_bits.allow & action.bit() != 0 || label_bits.allow & level.bit() != 0;
        let deny = ns_bits.deny & action.bit() != 0 || label_bits.deny & level.bit() != 0;
        (allow, deny)
    }

    /// May the account do `action` (read, create, update or delete) on the
    /// nodes (or edges) with `label` in `ns`?
    pub fn on_label(&self, action: Act, ns: &str, label: &str, edges: bool) -> bool {
        if self.superuser {
            return true;
        }
        let (allow, deny) = self.label_bits(action, ns, label, edges);
        allow && !deny
    }

    /// May the account do `action` (read, create, update or delete) on the
    /// property `property` of nodes with `label` in `ns`?
    pub fn on_property(&self, action: Act, ns: &str, label: &str, property: &str) -> bool {
        if self.superuser {
            return true;
        }
        let level = if action == Act::Read {
            Act::Read
        } else {
            Act::Write
        };
        let (label_allow, label_deny) = self.label_bits(action, ns, label, false);
        let bits = self
            .properties
            .bits(ns, &(label.to_owned(), property.to_owned()));
        let allow = label_allow || bits.allow & level.bit() != 0;
        let deny = label_deny || bits.deny & level.bit() != 0;
        allow && !deny
    }
}

fn all_roles(table: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<Vec<RoleDef>> {
    let mut out = Vec::new();
    for item in table.iter().map_err(storage)? {
        let (_, value) = item.map_err(storage)?;
        out.push(decode(value.value())?);
    }
    Ok(out)
}

/// Checks a role's name: what accounts and other roles name it by.
pub fn check_role_name(name: &str) -> Result<()> {
    crate::access::check_principal_name(name)?;
    if BUILT_IN_ROLES.contains(&name) {
        return Err(Error::Invalid(format!("{name:?} is a built-in role")));
    }
    Ok(())
}

impl ReadTx {
    /// Every defined role, in name order.
    pub fn roles(&self) -> Result<Vec<RoleDef>> {
        all_roles(&self.tx.open_table(ROLES).map_err(storage)?)
    }

    pub fn role(&self, name: &str) -> Result<Option<RoleDef>> {
        self.tx
            .open_table(ROLES)
            .map_err(storage)?
            .get(name)
            .map_err(storage)?
            .map(|guard| decode(guard.value()))
            .transpose()
    }

    /// Changes to accounts and roles so far: rights computed at one
    /// generation hold until it moves.
    pub fn access_generation(&self) -> Result<u64> {
        Ok(self
            .tx
            .open_table(META)
            .map_err(storage)?
            .get(ACCESS_GENERATION)
            .map_err(storage)?
            .map_or(0, |guard| guard.value()))
    }

    /// What `account` may do.
    pub fn rights(&self, account: &Account) -> Result<Rights> {
        let roles = self
            .roles()?
            .into_iter()
            .map(|role| (role.name.clone(), role))
            .collect();
        Ok(Rights::compute(account, &roles))
    }
}

impl WriteTx {
    pub fn roles(&self) -> Result<Vec<RoleDef>> {
        all_roles(&self.tx.open_table(ROLES).map_err(storage)?)
    }

    pub fn role(&self, name: &str) -> Result<Option<RoleDef>> {
        self.tx
            .open_table(ROLES)
            .map_err(storage)?
            .get(name)
            .map_err(storage)?
            .map(|guard| decode(guard.value()))
            .transpose()
    }

    /// Notes that accounts or roles changed: rights computed before are
    /// stale.
    pub(crate) fn bump_access(&mut self) -> Result<()> {
        let mut meta = self.tx.open_table(META).map_err(storage)?;
        let current = meta
            .get(ACCESS_GENERATION)
            .map_err(storage)?
            .map_or(0, |guard| guard.value());
        meta.insert(ACCESS_GENERATION, current + 1)
            .map_err(storage)?;
        Ok(())
    }

    /// Writes a role (created or changed). Its privileges must make sense,
    /// the roles it has must exist, and it must not end up having itself.
    pub fn put_role(&mut self, role: &RoleDef) -> Result<()> {
        check_role_name(&role.name)?;
        for privilege in &role.privileges {
            privilege.check()?;
        }
        for policy in &role.policies {
            if policy.namespace.trim().is_empty() {
                return Err(Error::Invalid("a policy needs a namespace pattern".into()));
            }
            if let Condition::Property { key, .. } = &policy.condition
                && key.trim().is_empty()
            {
                return Err(Error::Invalid(
                    "a policy compares a property: name it".into(),
                ));
            }
        }
        for name in &role.roles {
            if name != &role.name && self.role(name)?.is_none() {
                return Err(Error::Invalid(format!("there is no role {name:?}")));
            }
        }
        // A cycle: the role reached again from the roles it has (only the
        // roles below it are read).
        let mut seen = HashSet::new();
        let mut pending: Vec<String> = role.roles.clone();
        while let Some(name) = pending.pop() {
            if name == role.name {
                return Err(Error::Invalid(format!(
                    "the role {:?} would have itself (a cycle)",
                    role.name
                )));
            }
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(below) = self.role(&name)? {
                pending.extend(below.roles);
            }
        }
        let existed = self.role(&role.name)?.is_some();
        self.note(crate::replication::Touch::Role(role.name.clone()));
        self.tx
            .open_table(ROLES)
            .map_err(storage)?
            .insert(role.name.as_str(), encode(role)?.as_slice())
            .map_err(storage)?;
        self.bump_access()?;
        // Grants, denials and policies are changes of the role: the event
        // holds what it is now.
        self.audit_access(
            if existed {
                "role_changed"
            } else {
                "role_created"
            },
            &role.name,
            serde_json::json!({
                "privileges": role.privileges,
                "grants": role.grants,
                "roles": role.roles,
                "policies": role.policies,
            }),
        )
    }

    /// Removes a role, and every account's and role's grant of it; returns
    /// whether it existed.
    pub fn delete_role(&mut self, name: &str) -> Result<bool> {
        for mut role in self.roles()? {
            if role.roles.iter().any(|r| r == name) {
                role.roles.retain(|r| r != name);
                self.note(crate::replication::Touch::Role(role.name.clone()));
                self.tx
                    .open_table(ROLES)
                    .map_err(storage)?
                    .insert(role.name.as_str(), encode(&role)?.as_slice())
                    .map_err(storage)?;
                self.audit_access(
                    "role_changed",
                    &role.name,
                    serde_json::json!({ "roles": role.roles, "lost": name }),
                )?;
            }
        }
        for mut account in self.accounts()? {
            if account.roles.iter().any(|r| r == name) {
                account.roles.retain(|r| r != name);
                self.put_account(&account)?;
            }
        }
        self.note(crate::replication::Touch::Role(name.to_owned()));
        let removed = self
            .tx
            .open_table(ROLES)
            .map_err(storage)?
            .remove(name)
            .map_err(storage)?
            .is_some();
        self.bump_access()?;
        if removed {
            self.audit_access("role_deleted", name, serde_json::Value::Null)?;
        }
        Ok(removed)
    }

    /// Gives `role` privileges (grants or denials) and built-in levels on
    /// namespace patterns; returns how many it did not have yet.
    pub fn grant(
        &mut self,
        role: &str,
        privileges: &[Privilege],
        levels: &[Grant],
    ) -> Result<usize> {
        let mut def = self.existing_role(role)?;
        let mut added = 0;
        for privilege in privileges {
            privilege.check()?;
            if !def.privileges.contains(privilege) {
                def.privileges.push(privilege.clone());
                added += 1;
            }
        }
        for level in levels {
            if !def.grants.contains(level) {
                def.grants.push(level.clone());
                added += 1;
            }
        }
        if added > 0 {
            self.put_role(&def)?;
        }
        Ok(added)
    }

    /// Takes privileges (`which` says whether grants, denials or both) and
    /// built-in levels from `role`; returns how many it had.
    pub fn revoke(
        &mut self,
        role: &str,
        privileges: &[(Act, Object)],
        levels: &[Grant],
        which: Revoke,
    ) -> Result<usize> {
        let mut def = self.existing_role(role)?;
        let before = def.privileges.len() + def.grants.len();
        def.privileges.retain(|p| {
            let matches = privileges
                .iter()
                .any(|(action, object)| *action == p.action && *object == p.object);
            let kind = match which {
                Revoke::Both => true,
                Revoke::Grants => !p.deny,
                Revoke::Denials => p.deny,
            };
            !(matches && kind)
        });
        if which != Revoke::Denials {
            def.grants.retain(|g| !levels.contains(g));
        }
        let removed = before - def.privileges.len() - def.grants.len();
        if removed > 0 {
            self.put_role(&def)?;
        }
        Ok(removed)
    }

    /// Gives `role` to `to`, an account or another role; says which it was
    /// and whether it is new to it.
    pub fn grant_role(&mut self, role: &str, to: &str) -> Result<(Holder, bool)> {
        self.existing_role(role)?;
        if let Some(mut account) = self.account(to)? {
            if account.roles.iter().any(|r| r == role) {
                return Ok((Holder::Account, false));
            }
            account.roles.push(role.to_owned());
            self.put_account(&account)?;
            return Ok((Holder::Account, true));
        }
        if let Some(mut def) = self.role(to)? {
            if def.roles.iter().any(|r| r == role) {
                return Ok((Holder::Role, false));
            }
            def.roles.push(role.to_owned());
            self.put_role(&def)?;
            return Ok((Holder::Role, true));
        }
        Err(Error::Invalid(format!(
            "there is no account or role {to:?}"
        )))
    }

    /// Takes `role` from `from`, an account or another role; says which it
    /// was and whether it had it.
    pub fn revoke_role(&mut self, role: &str, from: &str) -> Result<(Holder, bool)> {
        if let Some(mut account) = self.account(from)? {
            let had = account.roles.iter().any(|r| r == role);
            if had {
                account.roles.retain(|r| r != role);
                self.put_account(&account)?;
            }
            return Ok((Holder::Account, had));
        }
        if let Some(mut def) = self.role(from)? {
            let had = def.roles.iter().any(|r| r == role);
            if had {
                def.roles.retain(|r| r != role);
                self.put_role(&def)?;
            }
            return Ok((Holder::Role, had));
        }
        Err(Error::Invalid(format!(
            "there is no account or role {from:?}"
        )))
    }

    /// Whether `name` is an account, a role or neither.
    pub fn holder(&self, name: &str) -> Result<Option<Holder>> {
        Ok(if self.account(name)?.is_some() {
            Some(Holder::Account)
        } else if self.role(name)?.is_some() {
            Some(Holder::Role)
        } else {
            None
        })
    }

    fn existing_role(&self, name: &str) -> Result<RoleDef> {
        if BUILT_IN_ROLES.contains(&name) {
            return Err(Error::Invalid(format!(
                "{name:?} is built in: grant its privileges to a role of your own"
            )));
        }
        self.role(name)?.ok_or_else(|| {
            Error::Invalid(format!(
                "there is no role {name:?} (CREATE ROLE {name} first)"
            ))
        })
    }
}

/// What [`WriteTx::revoke`] takes away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Revoke {
    Grants,
    Denials,
    Both,
}

/// What holds a role: an account or another role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    Account,
    Role,
}

/// The built-in level a name stands for (`reader`, `editor`, `admin`).
pub fn level_of(name: &str) -> Option<Role> {
    match name.to_ascii_lowercase().as_str() {
        "reader" => Some(Role::Read),
        "editor" => Some(Role::Write),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

impl std::fmt::Display for Object {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Object::Server => f.write_str("server"),
            Object::Namespace { namespace } => write!(f, "namespace {namespace}"),
            Object::Label {
                namespace,
                label,
                edges,
            } => write!(
                f,
                "{} {label} in {namespace}",
                if *edges { "edges" } else { "nodes" }
            ),
            Object::Property {
                namespace,
                label,
                property,
            } => write!(f, "property {label}.{property} in {namespace}"),
        }
    }
}
