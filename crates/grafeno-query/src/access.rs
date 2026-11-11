//! Managing access in GQ (phase 14.3): users, roles and grants.
//!
//! `CREATE USER`, `CREATE SERVICE ACCOUNT`, `ALTER USER`, `DROP USER`,
//! `CREATE ROLE`, `DROP ROLE`, `GRANT` / `DENY` / `REVOKE` privileges `ON`
//! the server, a namespace pattern, the nodes or edges with a label, or a
//! property `TO` / `FROM` roles, `GRANT ROLE … TO` and `REVOKE ROLE … FROM`
//! accounts and roles, and `SHOW USERS`, `SHOW ROLES`, `SHOW PRIVILEGES
//! [FOR name]`, `SHOW CURRENT USER`.
//!
//! Who runs them comes in [`AccessContext`]: managing accounts needs
//! `manage_accounts`, managing roles and grants `manage_roles`, and only
//! superusers make, change or drop superusers. Without a context (the
//! library, a local shell) everything is allowed.

use std::sync::Arc;

use grafeno::{
    Account, AccountKind, Act, Grant, Holder, Object, Privilege, ReadTx, Revoke, RoleDef,
    Timestamp, WriteTx,
};

use crate::QueryError;
use crate::ast::Expr;
use crate::eval::Params;
use crate::exec::{QueryResult, WriteStats};
use crate::value::Value;

/// Turns a password into the hash an account keeps, after checking it
/// against the server's policy: `(account, password) -> hash`.
pub type PasswordHasher = Arc<dyn Fn(&str, &str) -> Result<String, String> + Send + Sync>;

/// Who runs an access statement, and what it may manage.
#[derive(Clone)]
pub struct AccessContext {
    /// The caller's account (`None`: the server's token, or a local shell).
    pub account: Option<String>,
    /// A superuser, unscoped: may make, change and drop superusers.
    pub superuser: bool,
    pub manage_accounts: bool,
    pub manage_roles: bool,
    /// May read the audit log (`audit` on the server), 14.5.
    pub read_audit: bool,
    /// The roles the caller reaches, for `SHOW CURRENT USER`.
    pub effective_roles: Vec<String>,
    /// For `SET PASSWORD`; without one, passwords cannot be set in GQ.
    pub hash_password: Option<PasswordHasher>,
}

impl AccessContext {
    /// Everything allowed (the library, a local shell), with a hasher.
    pub fn owner(hash_password: Option<PasswordHasher>) -> Self {
        Self {
            account: None,
            superuser: true,
            manage_accounts: true,
            manage_roles: true,
            read_audit: true,
            effective_roles: Vec::new(),
            hash_password,
        }
    }
}

impl std::fmt::Debug for AccessContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessContext")
            .field("account", &self.account)
            .field("superuser", &self.superuser)
            .field("manage_accounts", &self.manage_accounts)
            .field("manage_roles", &self.manage_roles)
            .finish_non_exhaustive()
    }
}

/// An access statement.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Access {
    CreateUser {
        name: String,
        service: bool,
        if_not_exists: bool,
        set: Vec<UserSet>,
    },
    AlterUser {
        name: String,
        set: Vec<UserSet>,
    },
    DropUser {
        name: String,
        if_exists: bool,
    },
    CreateRole {
        name: String,
        if_not_exists: bool,
        description: Option<Expr>,
    },
    DropRole {
        name: String,
        if_exists: bool,
    },
    /// `GRANT` (or `DENY`) privileges on a target to roles.
    Grant {
        deny: bool,
        privileges: Vec<String>,
        target: Target,
        roles: Vec<String>,
    },
    /// `REVOKE [GRANT | DENY]` privileges on a target from roles.
    Revoke {
        which: Revoke,
        privileges: Vec<String>,
        target: Target,
        roles: Vec<String>,
    },
    GrantRole {
        roles: Vec<String>,
        to: Vec<String>,
    },
    RevokeRole {
        roles: Vec<String>,
        from: Vec<String>,
    },
    ShowUsers,
    ShowRoles,
    ShowPrivileges {
        holder: Option<String>,
    },
    ShowCurrentUser,
    /// `RESTRICT role …` (adds a record policy) or `UNRESTRICT role …`
    /// (removes it), 14.4.
    Restrict {
        role: String,
        policy: grafeno::Policy,
        remove: bool,
    },
}

/// One `SET` of `CREATE USER` or `ALTER USER`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UserSet {
    Password(Expr),
    /// `SET STATUS ACTIVE` (false) or `SUSPENDED` / `DISABLED` (true).
    Disabled(bool),
    /// `SET EXPIRES <time>`, or `null` for never.
    Expires(Expr),
    Superuser(bool),
    Description(Expr),
    /// `SET ATTRIBUTE key = value` (null removes it), for record policies.
    Attribute(String, Expr),
}

/// What privileges are granted on.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Server,
    Namespace(String),
    Label {
        namespace: String,
        label: String,
        edges: bool,
    },
    Property {
        namespace: String,
        label: String,
        property: String,
    },
}

impl Target {
    fn object(&self) -> Object {
        match self {
            Target::Server => Object::Server,
            Target::Namespace(namespace) => Object::Namespace {
                namespace: namespace.clone(),
            },
            Target::Label {
                namespace,
                label,
                edges,
            } => Object::Label {
                namespace: namespace.clone(),
                label: label.clone(),
                edges: *edges,
            },
            Target::Property {
                namespace,
                label,
                property,
            } => Object::Property {
                namespace: namespace.clone(),
                label: label.clone(),
                property: property.clone(),
            },
        }
    }
}

impl Access {
    pub(crate) fn writes(&self) -> bool {
        !matches!(
            self,
            Access::ShowUsers
                | Access::ShowRoles
                | Access::ShowPrivileges { .. }
                | Access::ShowCurrentUser
        )
    }

    /// What `EXPLAIN` says the statement does.
    pub(crate) fn describe(&self) -> String {
        match self {
            Access::CreateUser { name, service, .. } => format!(
                "create the {} {name} (manage_accounts)",
                if *service { "service account" } else { "user" }
            ),
            Access::AlterUser { name, .. } => {
                format!("change the account {name} (manage_accounts)")
            }
            Access::DropUser { name, .. } => {
                format!("drop the account {name}, its keys and sessions (manage_accounts)")
            }
            Access::CreateRole { name, .. } => format!("create the role {name} (manage_roles)"),
            Access::DropRole { name, .. } => {
                format!("drop the role {name}, taking it from everyone (manage_roles)")
            }
            Access::Grant {
                deny,
                privileges,
                target,
                roles,
            } => format!(
                "{} {} on {} to {} (manage_roles)",
                if *deny { "deny" } else { "grant" },
                privileges.join(", "),
                target.object(),
                roles.join(", ")
            ),
            Access::Revoke {
                privileges,
                target,
                roles,
                ..
            } => format!(
                "revoke {} on {} from {} (manage_roles)",
                privileges.join(", "),
                target.object(),
                roles.join(", ")
            ),
            Access::GrantRole { roles, to } => format!(
                "grant the roles {} to {} (manage_accounts for accounts, manage_roles for roles)",
                roles.join(", "),
                to.join(", ")
            ),
            Access::RevokeRole { roles, from } => format!(
                "revoke the roles {} from {} (manage_accounts for accounts, manage_roles for roles)",
                roles.join(", "),
                from.join(", ")
            ),
            Access::ShowUsers => "list the accounts (manage_accounts)".into(),
            Access::ShowRoles => "list the roles (manage_roles)".into(),
            Access::ShowPrivileges { holder: None } => "list every privilege".into(),
            Access::ShowPrivileges { holder: Some(name) } => format!(
                "list the privileges of {name} (manage_roles or manage_accounts, unless it is the caller)"
            ),
            Access::ShowCurrentUser => "describe the caller".into(),
            Access::Restrict {
                role,
                policy,
                remove,
            } => format!(
                "{} the policy on {policy} {} {role} (manage_roles)",
                if *remove { "remove" } else { "add" },
                if *remove { "from" } else { "to" }
            ),
        }
    }

    /// The privilege the caller lacks to run it, if any (for `EXPLAIN`).
    /// `GRANT ROLE` and `REVOKE ROLE` are checked as they run, by what
    /// each name turns out to be.
    pub(crate) fn lacking(&self, context: Option<&AccessContext>) -> Option<&'static str> {
        let context = context?;
        let accounts = || (!context.manage_accounts).then_some("manage_accounts");
        let roles = || (!context.manage_roles).then_some("manage_roles");
        match self {
            Access::CreateUser { .. }
            | Access::AlterUser { .. }
            | Access::DropUser { .. }
            | Access::ShowUsers => accounts(),
            Access::CreateRole { .. }
            | Access::DropRole { .. }
            | Access::Grant { .. }
            | Access::Revoke { .. }
            | Access::Restrict { .. }
            | Access::ShowRoles => roles(),
            Access::ShowPrivileges { holder: Some(name) }
                if context.account.as_deref() != Some(name.as_str())
                    && !context.manage_roles
                    && !context.manage_accounts =>
            {
                Some("manage_roles")
            }
            _ => None,
        }
    }
}

fn forbidden(message: impl Into<String>) -> QueryError {
    QueryError {
        forbidden: true,
        ..QueryError::runtime(message)
    }
}

fn require(have: bool, privilege: &str, context: &AccessContext) -> Result<(), QueryError> {
    if have {
        Ok(())
    } else {
        Err(forbidden(format!(
            "{} has no `{privilege}` privilege on the server",
            match &context.account {
                Some(name) => format!("the account {name:?}"),
                None => "the caller".to_owned(),
            }
        )))
    }
}

fn table(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        columns: columns.iter().map(|c| (*c).to_owned()).collect(),
        rows,
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    }
}

fn done(message: String) -> QueryResult {
    table(&["result"], vec![vec![Value::Str(message)]])
}

fn strings(list: &[String]) -> Value {
    Value::List(list.iter().cloned().map(Value::Str).collect())
}

fn level_name(grant: &Grant) -> String {
    grafeno::level_name(grant.role).to_owned()
}

/// Runs an access statement.
pub(crate) fn run(
    read: &ReadTx,
    tx: Option<&mut WriteTx>,
    access: &Access,
    params: &Params,
    context: Option<&AccessContext>,
) -> Result<QueryResult, QueryError> {
    let owner = AccessContext::owner(None);
    let context = context.unwrap_or(&owner);
    if !access.writes() {
        return show(read, access, context);
    }
    let tx = tx.ok_or_else(|| QueryError::runtime("this statement needs a writable database"))?;
    let value = |expr: &Expr| -> Result<Value, QueryError> {
        crate::plan::constant(expr, Some(params))
            .ok_or_else(|| QueryError::runtime("a value here must be a literal or a parameter"))
    };
    match access {
        Access::CreateUser {
            name,
            service,
            if_not_exists,
            set,
        } => {
            require(context.manage_accounts, "manage_accounts", context)?;
            if tx.account(name)?.is_some() {
                if *if_not_exists {
                    return Ok(done(format!("the account {name} exists")));
                }
                return Err(QueryError::runtime(format!(
                    "an account named {name:?} exists"
                )));
            }
            let mut account = Account {
                name: name.clone(),
                kind: if *service {
                    AccountKind::Service
                } else {
                    AccountKind::User
                },
                password_hash: None,
                grants: Vec::new(),
                roles: Vec::new(),
                superuser: false,
                own_only: false,
                disabled: false,
                expires_at: None,
                attributes: Default::default(),
                description: None,
                created_at: Timestamp::now(),
            };
            apply_sets(&mut account, set, &value, context)?;
            if account.kind == AccountKind::User && account.password_hash.is_none() {
                return Err(QueryError::runtime(format!(
                    "a user needs a password: CREATE USER {name} SET PASSWORD $password (or CREATE SERVICE ACCOUNT {name}, which uses keys)"
                )));
            }
            tx.create_account(&account)?;
            Ok(done(format!(
                "created the {} {name}",
                if *service { "service account" } else { "user" }
            )))
        }
        Access::AlterUser { name, set } => {
            require(context.manage_accounts, "manage_accounts", context)?;
            let mut account = tx
                .account(name)?
                .ok_or_else(|| QueryError::runtime(format!("there is no account {name:?}")))?;
            if account.superuser && !context.superuser {
                return Err(forbidden("only superusers change a superuser"));
            }
            let was_disabled = account.disabled;
            let had_password = account.password_hash.clone();
            apply_sets(&mut account, set, &value, context)?;
            tx.put_account(&account)?;
            // Disabling it or a new password ends its sessions.
            if (account.disabled && !was_disabled) || account.password_hash != had_password {
                let ended: Vec<String> = tx
                    .all_sessions()?
                    .into_iter()
                    .filter(|s| s.account == *name)
                    .map(|s| s.id)
                    .collect();
                for id in ended {
                    tx.delete_session(&id)?;
                }
            }
            Ok(done(format!("changed the account {name}")))
        }
        Access::DropUser { name, if_exists } => {
            require(context.manage_accounts, "manage_accounts", context)?;
            match tx.account(name)? {
                None if *if_exists => Ok(done(format!("there is no account {name}"))),
                None => Err(QueryError::runtime(format!("there is no account {name:?}"))),
                Some(account) => {
                    if account.superuser && !context.superuser {
                        return Err(forbidden("only superusers drop a superuser"));
                    }
                    tx.delete_account(name)?;
                    Ok(done(format!("dropped the account {name}")))
                }
            }
        }
        Access::CreateRole {
            name,
            if_not_exists,
            description,
        } => {
            require(context.manage_roles, "manage_roles", context)?;
            if tx.role(name)?.is_some() {
                if *if_not_exists {
                    return Ok(done(format!("the role {name} exists")));
                }
                return Err(QueryError::runtime(format!("a role named {name:?} exists")));
            }
            let description = match description {
                Some(expr) => match value(expr)? {
                    Value::Str(text) => Some(text),
                    Value::Null => None,
                    _ => return Err(QueryError::runtime("a description is a string")),
                },
                None => None,
            };
            tx.put_role(&RoleDef {
                name: name.clone(),
                privileges: Vec::new(),
                grants: Vec::new(),
                roles: Vec::new(),
                policies: Vec::new(),
                description,
                created_at: Timestamp::now(),
            })?;
            Ok(done(format!("created the role {name}")))
        }
        Access::DropRole { name, if_exists } => {
            require(context.manage_roles, "manage_roles", context)?;
            if tx.delete_role(name)? {
                Ok(done(format!("dropped the role {name}")))
            } else if *if_exists {
                Ok(done(format!("there is no role {name}")))
            } else {
                Err(QueryError::runtime(format!("there is no role {name:?}")))
            }
        }
        Access::Grant {
            deny,
            privileges,
            target,
            roles,
        } => {
            require(context.manage_roles, "manage_roles", context)?;
            let (privileges, levels) = resolve(privileges, target, *deny)?;
            let mut added = 0;
            for role in roles {
                added += tx.grant(role, &privileges, &levels)?;
            }
            Ok(done(format!(
                "{} {added} privilege{} on {} to {}",
                if *deny { "denied" } else { "granted" },
                if added == 1 { "" } else { "s" },
                target.object(),
                roles.join(", ")
            )))
        }
        Access::Revoke {
            which,
            privileges,
            target,
            roles,
        } => {
            require(context.manage_roles, "manage_roles", context)?;
            let (resolved, levels) = resolve(privileges, target, *which == Revoke::Denials)?;
            let pairs: Vec<(Act, Object)> =
                resolved.into_iter().map(|p| (p.action, p.object)).collect();
            let mut removed = 0;
            for role in roles {
                removed += tx.revoke(role, &pairs, &levels, *which)?;
            }
            Ok(done(format!(
                "revoked {removed} privilege{} on {} from {}",
                if removed == 1 { "" } else { "s" },
                target.object(),
                roles.join(", ")
            )))
        }
        Access::GrantRole { roles, to } => {
            let mut changed = 0;
            for holder in to {
                check_holder(tx, holder, context)?;
                for role in roles {
                    changed += usize::from(tx.grant_role(role, holder)?.1);
                }
            }
            Ok(done(format!(
                "granted {} to {} ({changed} new)",
                roles.join(", "),
                to.join(", ")
            )))
        }
        Access::RevokeRole { roles, from } => {
            let mut changed = 0;
            for holder in from {
                check_holder(tx, holder, context)?;
                for role in roles {
                    changed += usize::from(tx.revoke_role(role, holder)?.1);
                }
            }
            Ok(done(format!(
                "revoked {} from {} ({changed} held)",
                roles.join(", "),
                from.join(", ")
            )))
        }
        Access::Restrict {
            role,
            policy,
            remove,
        } => {
            require(context.manage_roles, "manage_roles", context)?;
            let mut def = tx.role(role)?.ok_or_else(|| {
                QueryError::runtime(format!(
                    "there is no role {role:?} (CREATE ROLE {role} first)"
                ))
            })?;
            let had = def.policies.contains(policy);
            if *remove {
                def.policies.retain(|p| p != policy);
            } else if !had {
                def.policies.push(policy.clone());
            }
            if had == *remove {
                tx.put_role(&def)?;
            }
            Ok(done(format!(
                "{} the policy on {policy} {} {role}",
                match (remove, had) {
                    (true, true) => "removed",
                    (true, false) => "did not find",
                    (false, true) => "kept",
                    (false, false) => "added",
                },
                if *remove { "from" } else { "to" }
            )))
        }
        Access::ShowUsers
        | Access::ShowRoles
        | Access::ShowPrivileges { .. }
        | Access::ShowCurrentUser => unreachable!("handled above"),
    }
}

/// Granting a role to an account is managing accounts (and a superuser's
/// roles are only a superuser's to change); to a role, managing roles.
fn check_holder(tx: &WriteTx, name: &str, context: &AccessContext) -> Result<(), QueryError> {
    match tx.holder(name)? {
        Some(Holder::Account) => {
            require(context.manage_accounts, "manage_accounts", context)?;
            if tx.account(name)?.is_some_and(|a| a.superuser) && !context.superuser {
                return Err(forbidden("only superusers change a superuser"));
            }
            Ok(())
        }
        Some(Holder::Role) => require(context.manage_roles, "manage_roles", context),
        None => Err(QueryError::runtime(format!(
            "there is no account or role {name:?}"
        ))),
    }
}

/// Privileges named in a statement, as privileges and built-in levels.
/// A level (`reader`, `editor`, `admin`) applies to a namespace; denying
/// one denies each of its actions.
fn resolve(
    names: &[String],
    target: &Target,
    deny: bool,
) -> Result<(Vec<Privilege>, Vec<Grant>), QueryError> {
    let object = target.object();
    let mut privileges = Vec::new();
    let mut levels = Vec::new();
    for name in names {
        if let Some(role) = grafeno::level_of(name) {
            let Target::Namespace(namespace) = target else {
                return Err(QueryError::runtime(format!(
                    "{name} is a level on a namespace: GRANT {name} ON NAMESPACE …"
                )));
            };
            if deny {
                privileges.extend(grafeno::level_actions(role).iter().map(|action| Privilege {
                    action: *action,
                    object: object.clone(),
                    deny: true,
                }));
            } else {
                levels.push(Grant {
                    namespace: namespace.clone(),
                    role,
                });
            }
            continue;
        }
        let action = Act::parse(name).map_err(|_| {
            QueryError::runtime(format!(
                "unknown privilege {name:?} (SHOW PRIVILEGES lists them)"
            ))
        })?;
        let privilege = Privilege {
            action,
            object: object.clone(),
            deny,
        };
        privilege
            .check()
            .map_err(|error| QueryError::runtime(error.to_string()))?;
        privileges.push(privilege);
    }
    Ok((privileges, levels))
}

fn apply_sets(
    account: &mut Account,
    set: &[UserSet],
    value: &dyn Fn(&Expr) -> Result<Value, QueryError>,
    context: &AccessContext,
) -> Result<(), QueryError> {
    for item in set {
        match item {
            UserSet::Password(expr) => {
                if account.kind == AccountKind::Service {
                    return Err(QueryError::runtime(
                        "a service account has no password: give it API keys",
                    ));
                }
                let Value::Str(password) = value(expr)? else {
                    return Err(QueryError::runtime("a password is a string"));
                };
                let hasher = context.hash_password.as_ref().ok_or_else(|| {
                    QueryError::runtime(
                        "passwords are set through the server (or `grafeno users password`)",
                    )
                })?;
                account.password_hash =
                    Some(hasher(&account.name, &password).map_err(QueryError::runtime)?);
            }
            UserSet::Disabled(disabled) => account.disabled = *disabled,
            UserSet::Expires(expr) => {
                account.expires_at = match value(expr)? {
                    Value::Null => None,
                    Value::Time(at) => Some(at),
                    Value::Int(micros) => Some(Timestamp::from_micros(micros)),
                    Value::Str(text) => Some(
                        Timestamp::parse(&text)
                            .map_err(|error| QueryError::runtime(error.to_string()))?,
                    ),
                    _ => return Err(QueryError::runtime("an expiry is a time, or null")),
                }
            }
            UserSet::Superuser(superuser) => {
                if !context.superuser {
                    return Err(forbidden("only superusers make or unmake a superuser"));
                }
                account.superuser = *superuser;
            }
            UserSet::Attribute(key, expr) => {
                let value = value(expr)?;
                if value == Value::Null {
                    account.attributes.remove(key);
                } else {
                    account.attributes.insert(key.clone(), value.to_json());
                }
            }
            UserSet::Description(expr) => {
                account.description = match value(expr)? {
                    Value::Str(text) if !text.trim().is_empty() => Some(text),
                    Value::Str(_) | Value::Null => None,
                    _ => return Err(QueryError::runtime("a description is a string")),
                }
            }
        }
    }
    Ok(())
}

fn time(at: Option<Timestamp>) -> Value {
    at.map_or(Value::Null, Value::Time)
}

fn show(
    read: &ReadTx,
    access: &Access,
    context: &AccessContext,
) -> Result<QueryResult, QueryError> {
    match access {
        Access::ShowUsers => {
            require(context.manage_accounts, "manage_accounts", context)?;
            let rows = read
                .accounts()?
                .into_iter()
                .map(|a| {
                    vec![
                        Value::Str(a.name.clone()),
                        Value::Str(
                            match a.kind {
                                AccountKind::User => "user",
                                AccountKind::Service => "service",
                            }
                            .into(),
                        ),
                        strings(&a.roles),
                        Value::List(
                            a.grants
                                .iter()
                                .map(|g| {
                                    Value::Str(format!("{} on {}", level_name(g), g.namespace))
                                })
                                .collect(),
                        ),
                        Value::Bool(a.superuser),
                        Value::Str(
                            if a.disabled {
                                "disabled"
                            } else if a.expired(Timestamp::now()) {
                                "expired"
                            } else {
                                "active"
                            }
                            .into(),
                        ),
                        time(a.expires_at),
                        Value::Time(a.created_at),
                    ]
                })
                .collect();
            Ok(table(
                &[
                    "name",
                    "kind",
                    "roles",
                    "grants",
                    "superuser",
                    "status",
                    "expires_at",
                    "created_at",
                ],
                rows,
            ))
        }
        Access::ShowRoles => {
            require(context.manage_roles, "manage_roles", context)?;
            let mut rows: Vec<Vec<Value>> = [
                (
                    "reader",
                    "read, export, algorithms on a namespace (a level)",
                ),
                (
                    "editor",
                    "also create, update, delete, merge, retract (a level)",
                ),
                (
                    "admin",
                    "also import, schema, drop, create the namespace (a level)",
                ),
                ("superuser", "everything (an account's flag)"),
            ]
            .into_iter()
            .map(|(name, what)| {
                vec![
                    Value::Str(name.into()),
                    Value::Bool(true),
                    Value::List(Vec::new()),
                    Value::Int(0),
                    Value::Str(what.into()),
                ]
            })
            .collect();
            for role in read.roles()? {
                rows.push(vec![
                    Value::Str(role.name),
                    Value::Bool(false),
                    strings(&role.roles),
                    Value::Int((role.privileges.len() + role.grants.len()) as i64),
                    role.description.map_or(Value::Null, Value::Str),
                ]);
            }
            Ok(table(
                &["name", "built_in", "roles", "privileges", "description"],
                rows,
            ))
        }
        Access::ShowPrivileges { holder: None } => {
            let mut rows = Vec::new();
            for (on, actions) in [
                ("server", &Act::SERVER[..]),
                ("namespace", &Act::NAMESPACE[..]),
                ("label", &[Act::Read, Act::Write][..]),
                ("property", &[Act::Read, Act::Write][..]),
            ] {
                for action in actions {
                    rows.push(vec![Value::Str(action.name()), Value::Str(on.into())]);
                }
            }
            Ok(table(&["privilege", "on"], rows))
        }
        Access::ShowPrivileges { holder: Some(name) } => {
            if context.account.as_deref() != Some(name.as_str())
                && !context.manage_roles
                && !context.manage_accounts
            {
                return Err(forbidden(
                    "listing another account's or a role's privileges needs `manage_roles` or `manage_accounts`",
                ));
            }
            privileges_of(read, name)
        }
        Access::ShowCurrentUser => {
            let row = match &context.account {
                None => vec![
                    Value::Str("owner".into()),
                    Value::Str("token".into()),
                    Value::Bool(true),
                    Value::List(Vec::new()),
                    Value::List(Vec::new()),
                ],
                Some(name) => {
                    let account = read.account(name)?;
                    let mut reached = context.effective_roles.clone();
                    reached.sort();
                    vec![
                        Value::Str(name.clone()),
                        Value::Str(
                            match account.as_ref().map(|a| a.kind) {
                                Some(AccountKind::User) => "user",
                                _ => "service",
                            }
                            .into(),
                        ),
                        Value::Bool(account.as_ref().is_some_and(|a| a.superuser)),
                        strings(&account.map(|a| a.roles).unwrap_or_default()),
                        strings(&reached),
                    ]
                }
            };
            Ok(table(
                &["name", "kind", "superuser", "roles", "effective_roles"],
                vec![row],
            ))
        }
        _ => unreachable!("only SHOW statements read"),
    }
}

/// Every privilege an account or a role has, with the role it comes from.
fn privileges_of(read: &ReadTx, name: &str) -> Result<QueryResult, QueryError> {
    let mut rows = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    if let Some(account) = read.account(name)? {
        if account.superuser {
            rows.push(vec![
                Value::Str(name.to_owned()),
                Value::Str("grant".into()),
                Value::Str("superuser".into()),
                Value::Str("everything".into()),
            ]);
        }
        for grant in &account.grants {
            rows.push(vec![
                Value::Str(name.to_owned()),
                Value::Str("grant".into()),
                Value::Str(level_name(grant)),
                Value::Str(format!("namespace {}", grant.namespace)),
            ]);
        }
        pending.extend(account.roles);
        if read.role(grafeno::PUBLIC)?.is_some() {
            pending.push(grafeno::PUBLIC.to_owned());
        }
    } else if read.role(name)?.is_some() {
        pending.push(name.to_owned());
    } else {
        return Err(QueryError::runtime(format!(
            "there is no account or role {name:?}"
        )));
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(role) = pending.pop() {
        if !seen.insert(role.clone()) {
            continue;
        }
        let Some(def) = read.role(&role)? else {
            continue;
        };
        for grant in &def.grants {
            rows.push(vec![
                Value::Str(role.clone()),
                Value::Str("grant".into()),
                Value::Str(level_name(grant)),
                Value::Str(format!("namespace {}", grant.namespace)),
            ]);
        }
        for privilege in &def.privileges {
            rows.push(vec![
                Value::Str(role.clone()),
                Value::Str(if privilege.deny { "deny" } else { "grant" }.into()),
                Value::Str(privilege.action.name()),
                Value::Str(privilege.object.to_string()),
            ]);
        }
        for policy in &def.policies {
            rows.push(vec![
                Value::Str(role.clone()),
                Value::Str("policy".into()),
                Value::Str(
                    match policy.applies {
                        grafeno::Applies::Read => "read",
                        grafeno::Applies::Write => "write",
                        grafeno::Applies::All => "all",
                    }
                    .into(),
                ),
                Value::Str(policy.to_string()),
            ]);
        }
        pending.extend(def.roles);
    }
    Ok(table(&["source", "access", "privilege", "on"], rows))
}

/// A grant over the API, as [`manage_grants`] takes it: the same rules
/// as the GQ statements.
#[derive(Clone, Debug, PartialEq)]
pub enum GrantRequest {
    /// `GRANT` (or `DENY`) privileges (actions or levels) on an object to
    /// roles.
    Grant {
        deny: bool,
        privileges: Vec<String>,
        on: Object,
        roles: Vec<String>,
    },
    /// `REVOKE` privileges on an object from roles.
    Revoke {
        which: Revoke,
        privileges: Vec<String>,
        on: Object,
        roles: Vec<String>,
    },
    /// `GRANT ROLE … TO` accounts or roles.
    GrantRole { roles: Vec<String>, to: Vec<String> },
    /// `REVOKE ROLE … FROM` accounts or roles.
    RevokeRole {
        roles: Vec<String>,
        from: Vec<String>,
    },
    /// `SHOW PRIVILEGES FOR` an account or a role.
    Privileges { holder: String },
}

impl From<Object> for Target {
    fn from(object: Object) -> Self {
        match object {
            Object::Server => Target::Server,
            Object::Namespace { namespace } => Target::Namespace(namespace),
            Object::Label {
                namespace,
                label,
                edges,
            } => Target::Label {
                namespace,
                label,
                edges,
            },
            Object::Property {
                namespace,
                label,
                property,
            } => Target::Property {
                namespace,
                label,
                property,
            },
        }
    }
}

impl GrantRequest {
    /// Whether it changes anything (all but `Privileges`).
    pub fn writes(&self) -> bool {
        !matches!(self, GrantRequest::Privileges { .. })
    }
}

/// Runs a [`GrantRequest`] for a caller described by `context` (writing
/// ones need `tx`).
pub fn manage_grants(
    read: &ReadTx,
    tx: Option<&mut WriteTx>,
    request: GrantRequest,
    context: &AccessContext,
) -> Result<QueryResult, QueryError> {
    let access = match request {
        GrantRequest::Grant {
            deny,
            privileges,
            on,
            roles,
        } => Access::Grant {
            deny,
            privileges,
            target: on.into(),
            roles,
        },
        GrantRequest::Revoke {
            which,
            privileges,
            on,
            roles,
        } => Access::Revoke {
            which,
            privileges,
            target: on.into(),
            roles,
        },
        GrantRequest::GrantRole { roles, to } => Access::GrantRole { roles, to },
        GrantRequest::RevokeRole { roles, from } => Access::RevokeRole { roles, from },
        GrantRequest::Privileges { holder } => Access::ShowPrivileges {
            holder: Some(holder),
        },
    };
    run(read, tx, &access, &Params::new(), Some(context))
}
