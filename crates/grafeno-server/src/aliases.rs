//! Namespace aliases over HTTP: `/v1/{alias}/…` is served as
//! `/v1/{namespace}/…`, and `/v1/aliases` lists, points and drops them.
//!
//! A request through an alias is rewritten before it is routed, so rights,
//! limits, clusters and groups all see the namespace it stands for: an
//! alias is a name, not a boundary, and moving it never lets anyone read
//! what they could not read under the namespace's own name. Moving one is
//! one commit: requests resolved before it finish on the old namespace,
//! those after it go to the new one.

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use grafeno::{Act, Alias, Error};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::http::{ApiError, ApiResult, AppState, Body, write};

/// The header a response through an alias carries: the namespace served.
pub const ALIAS_HEADER: &str = "grafeno-namespace";

/// Serves `/v1/{alias}/…` as `/v1/{namespace}/…`.
pub async fn resolve(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let Some((name, rest)) = alias_part(request.uri().path()) else {
        return next.run(request).await;
    };
    let target = match state.db.resolve_alias(&name) {
        Ok(Some(target)) => target,
        Ok(None) => return next.run(request).await,
        Err(error) => return ApiError::from(error).into_response(),
    };
    let mut path = format!("/v1/{}/{rest}", crate::cluster::encode_segment(&target));
    if let Some(query) = request.uri().query() {
        path.push('?');
        path.push_str(query);
    }
    match path.parse::<Uri>() {
        Ok(uri) => {
            // What the routers inside take as the request's own path.
            request
                .extensions_mut()
                .insert(axum::extract::OriginalUri(uri.clone()));
            *request.uri_mut() = uri;
        }
        Err(_) => return next.run(request).await,
    }
    let mut response = next.run(request).await;
    if let Ok(value) = target.parse() {
        response.headers_mut().insert(ALIAS_HEADER, value);
    }
    response
}

/// The name in `/v1/{name}/{rest}` when it may be an alias: a route under
/// a namespace, not one of the API's own.
fn alias_part(path: &str) -> Option<(String, &str)> {
    let rest = path.strip_prefix("/v1/")?;
    let (first, rest) = rest.split_once('/')?;
    let name = auth::decode_segment(first);
    if rest.is_empty() || grafeno::RESERVED.contains(&name.as_str()) || name == "aliases" {
        return None;
    }
    Some((name, rest))
}

#[derive(Serialize)]
pub struct AliasOutput {
    pub name: String,
    pub namespace: String,
    pub updated_at: grafeno::Timestamp,
    /// The namespace it pointed at before this change, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

impl AliasOutput {
    fn of(alias: Alias, previous: Option<String>) -> Self {
        Self {
            name: alias.name,
            namespace: alias.namespace,
            updated_at: alias.updated_at,
            previous,
        }
    }
}

/// `GET /v1/aliases`: those whose namespace the caller may read.
pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<AliasOutput>>> {
    let caller = auth::current();
    let db = state.db.clone();
    let aliases = tokio::task::spawn_blocking(move || db.begin_read()?.aliases())
        .await
        .map_err(crate::http::joined)??;
    Ok(Json(
        aliases
            .into_iter()
            .filter(|alias| caller.may(&alias.namespace, grafeno::Role::Read))
            .map(|alias| AliasOutput::of(alias, None))
            .collect(),
    ))
}

/// `GET /v1/aliases/{name}`.
pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<AliasOutput>> {
    let caller = auth::current();
    let db = state.db.clone();
    let lookup = name.clone();
    let alias = tokio::task::spawn_blocking(move || db.begin_read()?.alias(&lookup))
        .await
        .map_err(crate::http::joined)??;
    match alias {
        Some(alias) if caller.may(&alias.namespace, grafeno::Role::Read) => {
            Ok(Json(AliasOutput::of(alias, None)))
        }
        _ => Err(ApiError::not_found_message(format!("no alias {name:?}"))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasInput {
    pub namespace: String,
}

/// The right to point an alias at `ns` or away from it: changing what a
/// name serves is a change to how that namespace is reached.
fn require_schema(ns: &str) -> ApiResult<()> {
    let caller = auth::current();
    if caller.can_all(&[Act::Schema], ns) {
        Ok(())
    } else {
        Err(ApiError::forbidden_message(
            caller.refusal(ns, &[Act::Schema]),
        ))
    }
}

/// `PUT /v1/aliases/{name}`: creates the alias or moves it, in one commit.
pub async fn put(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Body(input): Body<AliasInput>,
) -> ApiResult<(StatusCode, Json<AliasOutput>)> {
    require_schema(&input.namespace)?;
    // Moving it away needs the same on the namespace it leaves.
    let db = state.db.clone();
    let lookup = name.clone();
    let current = tokio::task::spawn_blocking(move || db.begin_read()?.alias(&lookup))
        .await
        .map_err(crate::http::joined)??;
    if let Some(current) = &current {
        require_schema(&current.namespace)?;
    }
    let target = input.namespace;
    let (alias, previous) = write(&state, move |tx| {
        let previous = tx.set_alias(&name, &target)?;
        let alias = tx
            .alias(&name)?
            .ok_or_else(|| Error::Conflict(format!("alias {name:?} changed meanwhile")))?;
        Ok((alias, previous))
    })
    .await?;
    let status = if previous.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(AliasOutput::of(alias, previous))))
}

/// `DELETE /v1/aliases/{name}`: the alias goes, its namespace stays.
pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let db = state.db.clone();
    let lookup = name.clone();
    let current = tokio::task::spawn_blocking(move || db.begin_read()?.alias(&lookup))
        .await
        .map_err(crate::http::joined)??;
    let Some(current) = current else {
        return Err(ApiError::not_found_message(format!("no alias {name:?}")));
    };
    require_schema(&current.namespace)?;
    write(&state, move |tx| tx.drop_alias(&name)).await?;
    Ok(StatusCode::NO_CONTENT)
}
