//! Accounts, sign-in and sessions over HTTP (phase 14.1): `/v1/auth/*`
//! (signing in, refreshing, signing out, sessions, one's password, an open
//! server's first account), `/v1/accounts` (superusers) and `/v1/keys`
//! (an account's API keys: created, scoped, rotated, revoked).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use grafeno::{
    Account, AccountKind, ApiKey, Grant, KeyScope, NewAuditEvent, Outcome, Session, Timestamp,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{self, Caller, Via};
use crate::http::{ApiError, ApiResult, AppState, Body};

/// How long credentials last, and when sign-ins are refused.
#[derive(Clone, Copy, Debug)]
pub struct AuthConfig {
    /// An access token's life.
    pub session: Duration,
    /// A refresh token's life (a session ends when it runs out).
    pub refresh: Duration,
    pub lockout: auth::Lockout,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session: Duration::from_secs(15 * 60),
            refresh: Duration::from_secs(30 * 24 * 3600),
            lockout: auth::Lockout::default(),
        }
    }
}

fn micros(duration: Duration) -> i64 {
    duration.as_micros() as i64
}

fn later(by: Duration) -> Timestamp {
    Timestamp::from_micros(Timestamp::now().as_micros() + micros(by))
}

/// The address a request came from, when the server knows it.
pub struct ClientAddress(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for ClientAddress {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0.ip().to_string()),
        ))
    }
}

/// A sign-in that did not open a session, in the audit log (14.5).
async fn failed_signin(
    state: &AppState,
    name: &str,
    address: &Option<String>,
    reason: &str,
    outcome: Outcome,
) {
    let mut event = NewAuditEvent::new("signin_failed")
        .target(name)
        .outcome(outcome)
        .detail(json!({ "reason": reason }));
    event.address = address.clone();
    crate::audit::record(state, event).await;
}

fn unauthorized(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::with_status(StatusCode::UNAUTHORIZED, code, message)
}

/// Who presents `token`: the static token, a session's access token, or
/// an API key. On a cluster member, a session made a moment ago on the
/// leader may still be on its way: it is looked for again for a second.
pub fn authenticate(state: &AppState, token: &str) -> Result<Caller, ApiError> {
    let db = &state.db;
    let read_account = |name: &str| -> Result<(Arc<Account>, Arc<grafeno::View>), ApiError> {
        let read = db.begin_read()?;
        let account = read
            .account(name)?
            .ok_or_else(|| unauthorized("unauthorized", "the account is gone"))?;
        if account.disabled {
            return Err(unauthorized("account_disabled", "the account is disabled"));
        }
        if account.expired(Timestamp::now()) {
            return Err(unauthorized("account_expired", "the account expired"));
        }
        let view = state.keys.view(&read, &account)?;
        Ok((Arc::new(account), view))
    };
    if let Some((id, secret)) = auth::split_token(token, auth::ACCESS_PREFIX) {
        let mut session = db.begin_read()?.session(id)?;
        if session.is_none() && state.writer.cluster().is_some() {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(50));
                session = db.begin_read()?.session(id)?;
                if session.is_some() {
                    break;
                }
            }
        }
        let session = session.ok_or_else(|| unauthorized("unauthorized", "the session ended"))?;
        if !crate::http::constant_time_eq(
            auth::hash_secret(secret).as_bytes(),
            session.token_hash.as_bytes(),
        ) {
            return Err(unauthorized(
                "unauthorized",
                "the access token was replaced: refresh again",
            ));
        }
        if session.expires_at <= Timestamp::now() {
            return Err(unauthorized(
                "session_expired",
                "the access token expired: refresh the session",
            ));
        }
        let (account, view) = read_account(&session.account)?;
        return Ok(Caller::Account {
            account,
            view,
            scope: None,
            via: Via::Session(session.id),
        });
    }
    let hash = auth::hash_secret(token);
    let mut name = state.keys.key_name(db, &hash);
    // On a cluster member, a key made a moment ago may still be on its way.
    if name.is_none() && state.writer.cluster().is_some() {
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(210));
            name = state.keys.key_name(db, &hash);
            if name.is_some() {
                break;
            }
        }
    }
    let name = name
        .ok_or_else(|| unauthorized("unauthorized", "missing or wrong bearer token or API key"))?;
    let key = db
        .begin_read()?
        .api_key(&name)?
        .filter(|key| key.secret_hash == hash)
        .ok_or_else(|| unauthorized("unauthorized", "the key was revoked or rotated"))?;
    if key.expired(Timestamp::now()) {
        return Err(unauthorized(
            "key_expired",
            format!("the key {name:?} expired"),
        ));
    }
    let (account, view) = read_account(&key.account)?;
    Ok(Caller::Account {
        account,
        view,
        scope: key.scope.clone(),
        via: Via::Key(key.name),
    })
}

// ------------------------------------------------------------ outputs

/// An account over the API: never its password's hash.
#[derive(Serialize)]
pub struct AccountOutput {
    name: String,
    kind: AccountKind,
    has_password: bool,
    grants: Vec<Grant>,
    /// Roles granted to it (14.2).
    roles: Vec<String>,
    superuser: bool,
    /// `superuser`, under its name before 14.1.
    owner: bool,
    own_only: bool,
    disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    attributes: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    created_at: Timestamp,
}

impl From<&Account> for AccountOutput {
    fn from(account: &Account) -> Self {
        Self {
            name: account.name.clone(),
            kind: account.kind,
            has_password: account.password_hash.is_some(),
            grants: account.grants.clone(),
            roles: account.roles.clone(),
            superuser: account.superuser,
            owner: account.superuser,
            own_only: account.own_only,
            disabled: account.disabled,
            expires_at: account.expires_at,
            attributes: account.attributes.clone(),
            description: account.description.clone(),
            created_at: account.created_at,
        }
    }
}

/// A key over the API: never its secret or its hash. `grants`, `owner`
/// and `own_only` are its account's (as keys showed them before 14.1).
#[derive(Serialize)]
pub struct KeyOutput {
    name: String,
    account: String,
    grants: Vec<Grant>,
    owner: bool,
    own_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<KeyScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    created_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    rotated_at: Option<Timestamp>,
}

fn key_output(key: &ApiKey, account: Option<&Account>) -> KeyOutput {
    KeyOutput {
        name: key.name.clone(),
        account: key.account.clone(),
        grants: account.map(|a| a.grants.clone()).unwrap_or_default(),
        owner: account.is_some_and(|a| a.superuser),
        own_only: account.is_some_and(|a| a.own_only),
        scope: key.scope.clone(),
        expires_at: key.expires_at,
        description: key
            .description
            .clone()
            .or_else(|| account.and_then(|a| a.description.clone())),
        created_at: key.created_at,
        rotated_at: key.rotated_at,
    }
}

#[derive(Serialize)]
struct SessionOutput {
    id: String,
    account: String,
    created_at: Timestamp,
    expires_at: Timestamp,
    refresh_expires_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    refreshed_at: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    /// The session of the request that lists it.
    current: bool,
}

fn session_output(session: &Session, caller: &Caller) -> SessionOutput {
    SessionOutput {
        id: session.id.clone(),
        account: session.account.clone(),
        created_at: session.created_at,
        expires_at: session.expires_at,
        refresh_expires_at: session.refresh_expires_at,
        refreshed_at: session.refreshed_at,
        address: session.address.clone(),
        current: matches!(caller, Caller::Account { via: Via::Session(id), .. } if *id == session.id),
    }
}

/// A new session's tokens, shown once.
fn tokens(session: &Session, access: &str, refresh: &str, account: &Account) -> Value {
    let seconds =
        |at: Timestamp| (at.as_micros() - Timestamp::now().as_micros()).max(0) / 1_000_000;
    json!({
        "access_token": access,
        "expires_in": seconds(session.expires_at),
        "refresh_token": refresh,
        "refresh_expires_in": seconds(session.refresh_expires_at),
        "session": session.id,
        "account": AccountOutput::from(account),
    })
}

// ------------------------------------------------------------ handlers

fn me(caller: &Caller) -> ApiResult<Arc<Account>> {
    caller
        .account()
        .cloned()
        .ok_or_else(|| ApiError::bad_request("this needs an account (not the server's token)"))
}

/// `GET /v1/auth/status` (public): whether the server is open, and
/// whether its first account can be made by signing up.
pub async fn status(State(state): State<AppState>) -> Json<Value> {
    let accounts = state.keys.has_accounts();
    Json(json!({
        "open": state.token.is_none() && !accounts,
        "setup": state.token.is_none() && !accounts,
        "password_sign_in": true,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    name: String,
    password: String,
}

/// Makes a session for `account` and returns its tokens.
async fn open_session(
    state: &AppState,
    account: &Account,
    address: Option<String>,
) -> ApiResult<Value> {
    let id = auth::new_session_id();
    let (access, access_secret) = auth::new_token(auth::ACCESS_PREFIX, &id);
    let (refresh, refresh_secret) = auth::new_token(auth::REFRESH_PREFIX, &id);
    let session = Session {
        id,
        account: account.name.clone(),
        token_hash: auth::hash_secret(&access_secret),
        expires_at: later(state.auth.session),
        refresh_hash: auth::hash_secret(&refresh_secret),
        refresh_expires_at: later(state.auth.refresh),
        created_at: Timestamp::now(),
        refreshed_at: None,
        address,
    };
    let stored = session.clone();
    crate::http::write(state, move |tx| {
        tx.prune_sessions(&stored.account, Timestamp::now())?;
        tx.put_session(&stored)
    })
    .await?;
    Ok(tokens(&session, &access, &refresh, account))
}

/// `POST /v1/auth/setup` (public): on an open server with no accounts,
/// makes the first one, a superuser, and signs it in.
pub async fn setup(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    Body(input): Body<Credentials>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if state.token.is_some() || state.keys.has_accounts() {
        return Err(ApiError::forbidden_message(
            "this server already has accounts (or a token): sign in instead",
        ));
    }
    grafeno::check_principal_name(&input.name)?;
    auth::check_password(&input.name, &input.password).map_err(ApiError::bad_request)?;
    let account = Account {
        name: input.name.clone(),
        kind: AccountKind::User,
        password_hash: Some(auth::hash_password(&input.password)),
        grants: Vec::new(),
        roles: Vec::new(),
        superuser: true,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: Some("the first account".into()),
        created_at: Timestamp::now(),
    };
    let stored = account.clone();
    crate::http::write(&state, move |tx| {
        // Another first sign-up won the race.
        if !tx.accounts()?.is_empty() {
            return Err(grafeno::Error::Conflict(
                "the server already has an account".into(),
            ));
        }
        tx.create_account(&stored)
    })
    .await?;
    state.keys.note_account();
    let tokens = open_session(&state, &account, address).await?;
    Ok((StatusCode::CREATED, Json(tokens)))
}

/// `POST /v1/auth/signin` (public): a user's name and password for a
/// session. Failures lock the account, and the address, for a while.
pub async fn signin(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    Body(input): Body<Credentials>,
) -> ApiResult<Json<Value>> {
    let throttle = state.throttle.clone();
    if input.name.len() > auth::MAX_NAME_BYTES || input.password.len() > auth::MAX_PASSWORD_BYTES {
        return Err(unauthorized("unauthorized", "wrong name or password"));
    }
    // Counted before the password is checked, so that attempts in parallel
    // cannot all pass the limit.
    if let Err(message) = throttle.attempt(state.auth.lockout, &input.name, address.as_deref()) {
        failed_signin(&state, &input.name, &address, "locked", Outcome::Refused).await;
        return Err(ApiError::with_status(
            StatusCode::TOO_MANY_REQUESTS,
            "locked",
            message,
        ));
    }
    let db = state.db.clone();
    let name = input.name.clone();
    let password = input.password;
    let _turn = auth::password_checks()
        .acquire()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let account = tokio::task::spawn_blocking(move || -> grafeno::Result<Option<Account>> {
        let account = db.begin_read()?.account(&name)?;
        // A wrong name costs as much as a wrong password.
        let ok = match account.as_ref().and_then(|a| a.password_hash.as_deref()) {
            Some(hash) => auth::verify_password(hash, &password),
            None => {
                auth::verify_nothing(&password);
                false
            }
        };
        Ok(account.filter(|_| ok))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))??;
    drop(_turn);
    let Some(account) = account else {
        failed_signin(
            &state,
            &input.name,
            &address,
            "wrong name or password",
            Outcome::Failed,
        )
        .await;
        // This failure locks the account or the address: said once.
        if throttle
            .check(state.auth.lockout, &input.name, address.as_deref())
            .is_err()
        {
            let mut event = NewAuditEvent::new("lockout")
                .target(input.name.clone())
                .outcome(Outcome::Refused);
            event.address = address.clone();
            crate::audit::record(&state, event).await;
        }
        return Err(unauthorized("unauthorized", "wrong name or password"));
    };
    if account.disabled {
        failed_signin(&state, &input.name, &address, "disabled", Outcome::Refused).await;
        return Err(unauthorized("account_disabled", "the account is disabled"));
    }
    if account.expired(Timestamp::now()) {
        failed_signin(&state, &input.name, &address, "expired", Outcome::Refused).await;
        return Err(unauthorized("account_expired", "the account expired"));
    }
    throttle.succeeded(&account.name, address.as_deref());
    Ok(Json(open_session(&state, &account, address).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshInput {
    refresh_token: String,
}

/// `POST /v1/auth/refresh` (public): a refresh token for new tokens (both
/// replaced). A refresh token used twice ends the session: one of the two
/// users stole it.
pub async fn refresh(
    State(state): State<AppState>,
    Body(input): Body<RefreshInput>,
) -> ApiResult<Json<Value>> {
    let Some((id, secret)) = auth::split_token(&input.refresh_token, auth::REFRESH_PREFIX) else {
        return Err(unauthorized("unauthorized", "not a refresh token"));
    };
    let (id, hash) = (id.to_owned(), auth::hash_secret(secret));
    let session = state
        .db
        .begin_read()?
        .session(&id)?
        .ok_or_else(|| unauthorized("unauthorized", "the session ended"))?;
    if !crate::http::constant_time_eq(hash.as_bytes(), session.refresh_hash.as_bytes()) {
        let ended = id.clone();
        let account = session.account.clone();
        crate::http::write(&state, move |tx| {
            // A refresh token used twice: one of the two users stole it.
            tx.audit(
                NewAuditEvent::new("refresh_reused")
                    .target(account.clone())
                    .outcome(Outcome::Refused)
                    .detail(json!({ "session": ended })),
            )?;
            tx.delete_session(&ended)
        })
        .await?;
        return Err(unauthorized(
            "unauthorized",
            "the refresh token was already used: the session is ended",
        ));
    }
    if session.refresh_expires_at <= Timestamp::now() {
        return Err(unauthorized(
            "session_expired",
            "the session expired: sign in again",
        ));
    }
    let account = state
        .db
        .begin_read()?
        .account(&session.account)?
        .filter(|a| !a.disabled && !a.expired(Timestamp::now()))
        .ok_or_else(|| {
            unauthorized(
                "account_disabled",
                "the account is disabled, expired or gone",
            )
        })?;
    let (access, access_secret) = auth::new_token(auth::ACCESS_PREFIX, &id);
    let (refresh, refresh_secret) = auth::new_token(auth::REFRESH_PREFIX, &id);
    let renewed = Session {
        token_hash: auth::hash_secret(&access_secret),
        expires_at: later(state.auth.session),
        refresh_hash: auth::hash_secret(&refresh_secret),
        refreshed_at: Some(Timestamp::now()),
        ..session
    };
    let stored = renewed.clone();
    let expected = hash;
    crate::http::write(&state, move |tx| {
        // Two refreshes at once: only the first wins.
        match tx.session(&stored.id)? {
            Some(current) if current.refresh_hash == expected => tx.put_session(&stored),
            _ => Err(grafeno::Error::Conflict(
                "the session was refreshed meanwhile".into(),
            )),
        }
    })
    .await?;
    Ok(Json(tokens(&renewed, &access, &refresh, &account)))
}

/// `POST /v1/auth/signout`: ends the caller's session.
pub async fn signout(State(state): State<AppState>) -> ApiResult<StatusCode> {
    let Caller::Account {
        via: Via::Session(id),
        ..
    } = auth::current()
    else {
        return Err(ApiError::bad_request(
            "this request was not made with a session",
        ));
    };
    crate::http::write(&state, move |tx| tx.delete_session(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SessionsQuery {
    account: Option<String>,
}

/// `GET /v1/auth/sessions`: the caller's sessions (a superuser's: any
/// account's with `?account=`, or everyone's with `?account=*`).
pub async fn sessions(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<SessionsQuery>,
) -> ApiResult<Json<Vec<Value>>> {
    let caller = auth::current();
    // Superusers and those who manage accounts (14.5) list anyone's; the
    // sessions of superusers are only superusers' to see.
    let manages = caller.can_server(grafeno::Act::ManageAccounts);
    let account = match (&query.account, manages) {
        (Some(name), true) if name == "*" => None,
        (Some(name), true) => Some(name.clone()),
        (Some(name), false) if Some(name.as_str()) != caller.account().map(|a| a.name.as_str()) => {
            return Err(ApiError::forbidden_message(
                "listing other accounts' sessions needs the manage_accounts privilege",
            ));
        }
        _ => Some(me(&caller)?.name.clone()),
    };
    let read = state.db.begin_read()?;
    let mut list = read.sessions(account.as_deref())?;
    if !caller.is_owner() {
        let mine = caller.account().map(|a| a.name.clone());
        let mut kept = Vec::with_capacity(list.len());
        for session in list {
            let superuser = read.account(&session.account)?.is_some_and(|a| a.superuser);
            if !superuser || Some(&session.account) == mine.as_ref() {
                kept.push(session);
            }
        }
        list = kept;
    }
    Ok(Json(
        list.iter()
            .map(|s| serde_json::to_value(session_output(s, &caller)).unwrap_or(Value::Null))
            .collect(),
    ))
}

/// `DELETE /v1/auth/sessions/{id}`: ends one of the caller's sessions (a
/// superuser: anyone's).
pub async fn end_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let caller = auth::current();
    let session = state
        .db
        .begin_read()?
        .session(&id)?
        .ok_or_else(|| ApiError::not_found_message(format!("no session {id:?}")))?;
    let own = Some(session.account.as_str()) == caller.account().map(|a| a.name.as_str());
    let of_superuser = state
        .db
        .begin_read()?
        .account(&session.account)?
        .is_some_and(|a| a.superuser);
    let may = own
        || caller.is_owner()
        || (caller.can_server(grafeno::Act::ManageAccounts) && !of_superuser);
    if !may {
        return Err(ApiError::forbidden_message(
            "that session is another account's",
        ));
    }
    crate::http::write(&state, move |tx| tx.delete_session(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordInput {
    current_password: String,
    new_password: String,
}

/// `POST /v1/auth/password`: changes the caller's password (the current
/// one is asked again) and ends its other sessions.
pub async fn change_password(
    State(state): State<AppState>,
    ClientAddress(address): ClientAddress,
    Body(input): Body<PasswordInput>,
) -> ApiResult<StatusCode> {
    let caller = auth::current();
    let account = me(&caller)?;
    state
        .throttle
        .check(state.auth.lockout, &account.name, address.as_deref())
        .map_err(|message| {
            ApiError::with_status(StatusCode::TOO_MANY_REQUESTS, "locked", message)
        })?;
    if input.current_password.len() > auth::MAX_PASSWORD_BYTES {
        state.throttle.failed(&account.name, address.as_deref());
        return Err(unauthorized(
            "unauthorized",
            "the current password is wrong",
        ));
    }
    let _turn = auth::password_checks()
        .acquire()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let ok = account
        .password_hash
        .as_deref()
        .is_some_and(|hash| auth::verify_password(hash, &input.current_password));
    drop(_turn);
    if !ok {
        state.throttle.failed(&account.name, address.as_deref());
        return Err(unauthorized(
            "unauthorized",
            "the current password is wrong",
        ));
    }
    auth::check_password(&account.name, &input.new_password).map_err(ApiError::bad_request)?;
    let hash = auth::hash_password(&input.new_password);
    let keep = match &caller {
        Caller::Account {
            via: Via::Session(id),
            ..
        } => Some(id.clone()),
        _ => None,
    };
    let name = account.name.clone();
    crate::http::write(&state, move |tx| {
        let mut account = tx
            .account(&name)?
            .ok_or_else(|| grafeno::Error::Invalid("the account is gone".into()))?;
        account.password_hash = Some(hash.clone());
        tx.put_account(&account)?;
        for session in end_sessions(tx, &name, keep.as_deref())? {
            tx.delete_session(&session)?;
        }
        Ok(())
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The sessions of `account` but `keep`.
fn end_sessions(
    tx: &grafeno::WriteTx,
    account: &str,
    keep: Option<&str>,
) -> grafeno::Result<Vec<String>> {
    Ok(tx_sessions(tx, account)?
        .into_iter()
        .filter(|id| Some(id.as_str()) != keep)
        .collect())
}

/// `GET /v1/whoami`: who the caller is, how it signed in, and what it may
/// do.
pub async fn whoami() -> Json<Value> {
    Json(match auth::current() {
        Caller::Owner => {
            json!({ "name": "owner", "kind": "token", "owner": true, "superuser": true, "grants": [], "own_only": false })
        }
        Caller::Anonymous => json!({ "name": "anonymous" }),
        Caller::Account {
            account,
            view,
            scope,
            via,
        } => {
            let mut value =
                serde_json::to_value(AccountOutput::from(account.as_ref())).unwrap_or(Value::Null);
            // Every role it has, its own and those they have (14.2).
            let mut reached = view.rights.roles.clone();
            reached.sort();
            value["effective_roles"] = json!(reached);
            let (kind, name) = match via {
                Via::Key(name) => ("key", name),
                Via::Session(id) => ("session", id),
            };
            value["via"] = json!({ "kind": kind, "name": name });
            if let Some(scope) = scope {
                value["scope"] = json!(scope);
                value["owner"] = json!(false);
            }
            value
        }
    })
}

// ------------------------------------------------------------ accounts

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAccount {
    name: String,
    #[serde(default)]
    kind: AccountKind,
    password: Option<String>,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    superuser: bool,
    #[serde(default)]
    own_only: bool,
    description: Option<String>,
}

/// `GET /v1/accounts` (superusers).
pub async fn list_accounts(State(state): State<AppState>) -> ApiResult<Json<Vec<AccountOutput>>> {
    let accounts = state.db.begin_read()?.accounts()?;
    Ok(Json(accounts.iter().map(AccountOutput::from).collect()))
}

/// `POST /v1/accounts` (superusers): a user (with a password) or a service
/// account.
pub async fn create_account(
    State(state): State<AppState>,
    Body(input): Body<NewAccount>,
) -> ApiResult<(StatusCode, Json<AccountOutput>)> {
    let password_hash = match (input.kind, &input.password) {
        (AccountKind::User, Some(password)) => {
            auth::check_password(&input.name, password).map_err(ApiError::bad_request)?;
            Some(auth::hash_password(password))
        }
        (AccountKind::User, None) => return Err(ApiError::bad_request("a user needs a password")),
        (AccountKind::Service, Some(_)) => {
            return Err(ApiError::bad_request(
                "a service account has no password: give it API keys",
            ));
        }
        (AccountKind::Service, None) => None,
    };
    if input.superuser {
        only_superusers("make a superuser")?;
    }
    let account = Account {
        name: input.name,
        kind: input.kind,
        password_hash,
        grants: input.grants,
        roles: input.roles,
        superuser: input.superuser,
        own_only: input.own_only,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: input.description.filter(|d| !d.trim().is_empty()),
        created_at: Timestamp::now(),
    };
    let stored = account.clone();
    let (token, had) = (state.token.is_some(), state.keys.has_accounts());
    crate::http::write(&state, move |tx| {
        tx.create_account(&stored)?;
        keep_a_superuser(tx, token, had)
    })
    .await?;
    state.keys.note_account();
    Ok((StatusCode::CREATED, Json(AccountOutput::from(&account))))
}

/// `GET /v1/accounts/{name}` (superusers).
pub async fn get_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<AccountOutput>> {
    let account = state
        .db
        .begin_read()?
        .account(&name)?
        .ok_or_else(|| ApiError::not_found_message(format!("no account {name:?}")))?;
    Ok(Json(AccountOutput::from(&account)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountChange {
    password: Option<String>,
    grants: Option<Vec<Grant>>,
    roles: Option<Vec<String>>,
    superuser: Option<bool>,
    own_only: Option<bool>,
    disabled: Option<bool>,
    /// What record policies compare with (14.4); replaces them all.
    attributes: Option<serde_json::Map<String, Value>>,
    /// When it stops working (`null`: never).
    #[serde(default, deserialize_with = "crate::http::double_option")]
    expires_at: Option<Option<crate::api::TimeInput>>,
    description: Option<Option<String>>,
}

/// `PATCH /v1/accounts/{name}` (superusers): a new password, grants, or
/// status; disabling an account or setting its password ends its sessions.
pub async fn change_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Body(input): Body<AccountChange>,
) -> ApiResult<Json<AccountOutput>> {
    let hash = match &input.password {
        Some(password) => {
            auth::check_password(&name, password).map_err(ApiError::bad_request)?;
            Some(auth::hash_password(password))
        }
        None => None,
    };
    if input.superuser.is_some() {
        only_superusers("make or unmake a superuser")?;
    }
    guard_superuser(&state, &name)?;
    // Microseconds or an ISO 8601 date, as everywhere in the API.
    let expires_at = match &input.expires_at {
        Some(Some(at)) => Some(Some(at.resolve().map_err(ApiError::from)?)),
        Some(None) => Some(None),
        None => None,
    };
    let input = Arc::new(input);
    let target = name.clone();
    let (token, had) = (state.token.is_some(), state.keys.has_accounts());
    let changed = crate::http::write(&state, move |tx| {
        let mut account = tx
            .account(&target)?
            .ok_or_else(|| grafeno::Error::Invalid(format!("no account {target:?}")))?;
        let mut end = false;
        if let Some(hash) = &hash {
            if account.kind == AccountKind::Service {
                return Err(grafeno::Error::Invalid(
                    "a service account has no password".into(),
                ));
            }
            account.password_hash = Some(hash.clone());
            end = true;
        }
        if let Some(grants) = &input.grants {
            account.grants = grants.clone();
        }
        if let Some(roles) = &input.roles {
            account.roles = roles.clone();
        }
        if let Some(superuser) = input.superuser {
            account.superuser = superuser;
        }
        if let Some(own_only) = input.own_only {
            account.own_only = own_only;
        }
        if let Some(expires_at) = expires_at {
            account.expires_at = expires_at;
        }
        if let Some(attributes) = &input.attributes {
            account.attributes = attributes.clone();
        }
        if let Some(disabled) = input.disabled {
            end |= disabled && !account.disabled;
            account.disabled = disabled;
        }
        if let Some(description) = &input.description {
            account.description = description.clone();
        }
        tx.put_account(&account)?;
        keep_a_superuser(tx, token, had)?;
        if end {
            for session in tx_sessions(tx, &target)? {
                tx.delete_session(&session)?;
            }
        }
        Ok(account)
    })
    .await?;
    Ok(Json(AccountOutput::from(&changed)))
}

/// 403 unless the caller is a superuser: managing accounts (14.2) does not
/// reach superusers, or it would be a way to become one.
fn only_superusers(what: &str) -> ApiResult<()> {
    if auth::current().is_owner() {
        Ok(())
    } else {
        Err(ApiError::forbidden_message(format!(
            "only superusers {what}"
        )))
    }
}

/// 403 if `name` is a superuser and the caller is not.
fn guard_superuser(state: &AppState, name: &str) -> ApiResult<()> {
    let target = state.db.begin_read()?.account(name)?;
    if target.is_some_and(|account| account.superuser) {
        only_superusers("change or delete a superuser")?;
    }
    Ok(())
}

/// Without a token, accounts close the server: refuses a change that
/// would leave accounts but no active superuser, as nobody could manage
/// them any more (the first account must be one).
/// Dropping every account is refused too: it would open the server to
/// anyone (`had` says whether there were accounts before the change).
pub(crate) fn keep_a_superuser(
    tx: &grafeno::WriteTx,
    token: bool,
    had: bool,
) -> grafeno::Result<()> {
    if token {
        return Ok(());
    }
    let accounts = tx.accounts()?;
    if (accounts.is_empty() && !had) || accounts.iter().any(|a| a.superuser && !a.disabled) {
        Ok(())
    } else {
        Err(grafeno::Error::Invalid(
            "without a token in grafeno.toml, accounts close the server: keep an active superuser (the first account must be one)".into(),
        ))
    }
}

/// The ids of an account's sessions, inside a write.
fn tx_sessions(tx: &grafeno::WriteTx, account: &str) -> grafeno::Result<Vec<String>> {
    Ok(tx
        .all_sessions()?
        .into_iter()
        .filter(|s| s.account == account)
        .map(|s| s.id)
        .collect())
}

/// `DELETE /v1/accounts/{name}` (superusers): with its keys and sessions.
pub async fn delete_account(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    guard_superuser(&state, &name)?;
    let target = name.clone();
    let (token, had) = (state.token.is_some(), state.keys.has_accounts());
    let removed = crate::http::write(&state, move |tx| {
        let removed = tx.delete_account(&target)?;
        keep_a_superuser(tx, token, had)?;
        Ok(removed)
    })
    .await?;
    state.keys.reload(&state.db)?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found_message(format!("no account {name:?}")))
    }
}

// ------------------------------------------------------------ keys

/// `GET /v1/keys`: every key (superusers), or the caller's account's.
pub async fn list_keys(State(state): State<AppState>) -> ApiResult<Json<Vec<KeyOutput>>> {
    let caller = auth::current();
    let read = state.db.begin_read()?;
    let accounts: std::collections::HashMap<String, Account> = read
        .accounts()?
        .into_iter()
        .map(|a| (a.name.clone(), a))
        .collect();
    let mine = caller.account().map(|a| a.name.clone());
    Ok(Json(
        read.api_keys()?
            .iter()
            .filter(|key| caller.is_owner() || mine.as_deref() == Some(key.account.as_str()))
            .map(|key| key_output(key, accounts.get(&key.account)))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKey {
    name: String,
    /// The account it belongs to (the caller's when absent). Without one,
    /// `grants`, `owner` and `own_only` make a service account of the key's
    /// name (as keys were made before 14.1; superusers only).
    account: Option<String>,
    #[serde(default)]
    grants: Vec<Grant>,
    #[serde(default)]
    owner: bool,
    #[serde(default)]
    own_only: bool,
    scope: Option<KeyScope>,
    /// It stops working after this many days.
    expires_in_days: Option<u32>,
    description: Option<String>,
}

/// `POST /v1/keys`: a key and its secret, shown once.
pub async fn create_key(
    State(state): State<AppState>,
    Body(input): Body<NewKey>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let caller = auth::current();
    let legacy = input.account.is_none() && (!input.grants.is_empty() || input.owner);
    let account_name = match (&input.account, legacy) {
        (Some(name), _) => name.clone(),
        (None, true) => input.name.clone(),
        (None, false) => match caller.account() {
            Some(account) => account.name.clone(),
            None => {
                return Err(ApiError::bad_request(
                    "a key needs an account (`account`), or grants ({namespace, role}) or owner: true for a service account of its own",
                ));
            }
        },
    };
    let own = caller.account().is_some_and(|a| a.name == account_name);
    if !caller.is_owner() && (legacy || !own) {
        return Err(ApiError::forbidden_message(
            "only superusers make keys for other accounts",
        ));
    }
    // Without a token, accounts close the server: its first one must be a
    // superuser, or nobody could manage accounts any more.
    if legacy && state.token.is_none() && !input.owner {
        let superusers = state
            .db
            .begin_read()?
            .accounts()?
            .iter()
            .any(|a| a.superuser);
        if !superusers {
            return Err(ApiError::bad_request(
                "this server has no token: create an owner key first (owner: true), or the server would lock everyone out",
            ));
        }
    }
    let asked = input
        .scope
        .clone()
        .filter(|s| !s.namespaces.is_empty() || s.read_only);
    // A key made with a scoped key reaches no further than it: with no
    // scope asked it takes the caller's, and a wider one is refused. A
    // superuser's account would otherwise hand an unscoped key, and all
    // its power, to a key that was meant to read one namespace.
    let scope = match &caller {
        auth::Caller::Account {
            scope: Some(outer), ..
        } => Some(outer.narrowed(asked.as_ref()).ok_or_else(|| {
            ApiError::forbidden_message(
                "a key made with a scoped key cannot reach further than it: ask for a scope inside the caller's",
            )
        })?),
        _ => asked,
    };
    let secret = auth::new_secret();
    let key = ApiKey {
        name: input.name.clone(),
        account: account_name.clone(),
        secret_hash: auth::hash_secret(&secret),
        scope,
        expires_at: input
            .expires_in_days
            .map(|days| later(Duration::from_secs(u64::from(days) * 24 * 3600))),
        description: input.description.clone().filter(|d| !d.trim().is_empty()),
        created_at: Timestamp::now(),
        rotated_at: None,
    };
    let service = legacy.then(|| Account {
        name: input.name.clone(),
        kind: AccountKind::Service,
        password_hash: None,
        grants: input.grants.clone(),
        roles: Vec::new(),
        superuser: input.owner,
        own_only: input.own_only,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: key.description.clone(),
        created_at: key.created_at,
    });
    let stored = (service.clone(), key.clone());
    crate::http::write(&state, move |tx| {
        if let Some(account) = &stored.0 {
            tx.create_account(account)?;
        }
        tx.create_api_key(&stored.1)
    })
    .await?;
    state.keys.reload(&state.db)?;
    let account = state.db.begin_read()?.account(&account_name)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": key_output(&key, account.as_ref()), "secret": secret })),
    ))
}

/// The key `name`, if the caller may manage it (its account's, or a
/// superuser).
fn manageable(state: &AppState, name: &str) -> ApiResult<ApiKey> {
    let caller = auth::current();
    let key = state
        .db
        .begin_read()?
        .api_key(name)?
        .ok_or_else(|| ApiError::not_found_message(format!("no key named {name:?}")))?;
    if !caller.is_owner() && caller.account().map(|a| a.name.as_str()) != Some(key.account.as_str())
    {
        return Err(ApiError::forbidden_message("that key is another account's"));
    }
    Ok(key)
}

/// `POST /v1/keys/{name}/rotate`: a new secret, shown once; the old one
/// stops working.
pub async fn rotate_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let key = manageable(&state, &name)?;
    let secret = auth::new_secret();
    let rotated = ApiKey {
        secret_hash: auth::hash_secret(&secret),
        rotated_at: Some(Timestamp::now()),
        ..key
    };
    let stored = rotated.clone();
    crate::http::write(&state, move |tx| tx.put_api_key(&stored)).await?;
    state.keys.reload(&state.db)?;
    let account = state.db.begin_read()?.account(&rotated.account)?;
    Ok(Json(
        json!({ "key": key_output(&rotated, account.as_ref()), "secret": secret }),
    ))
}

/// `DELETE /v1/keys/{name}`: revokes a key at once. A key made the old way
/// (its service account of the same name, with no other key) takes its
/// account along.
pub async fn revoke_key(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let key = manageable(&state, &name)?;
    let target = name.clone();
    let (token, had) = (state.token.is_some(), state.keys.has_accounts());
    crate::http::write(&state, move |tx| {
        tx.delete_api_key(&target)?;
        let alone = key.account == target
            && tx
                .account(&target)?
                .is_some_and(|a| a.kind == AccountKind::Service)
            && !tx.all_api_keys()?.iter().any(|k| k.account == target);
        if alone {
            tx.delete_account(&target)?;
            keep_a_superuser(tx, token, had)?;
        }
        Ok(())
    })
    .await?;
    state.keys.reload(&state.db)?;
    Ok(StatusCode::NO_CONTENT)
}
