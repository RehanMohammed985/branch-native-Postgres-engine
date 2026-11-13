//! HTTP control API.
//!
//! ```text
//! GET    /v1/branches                 list branches
//! POST   /v1/branches                 {"name": "...", "from": "main"}
//! DELETE /v1/branches/{name}[?force=true]
//! POST   /v1/branches/{name}/stop     stop the branch's Postgres instance
//! POST   /v1/gc                       reclaim space from deleted branches
//! GET    /v1/status
//! ```

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::api_types::{ApiError, CreateBranch};
use crate::manager::{Manager, Refusal};

struct Failure(anyhow::Error);

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure(e)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.0.downcast_ref::<Refusal>() {
            Some(Refusal::NotFound(_)) => StatusCode::NOT_FOUND,
            Some(Refusal::Conflict(_)) => StatusCode::CONFLICT,
            Some(Refusal::Bad(_)) => StatusCode::BAD_REQUEST,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(ApiError { error: format!("{:#}", self.0) })).into_response()
    }
}

#[derive(Deserialize)]
struct Force {
    #[serde(default)]
    force: bool,
}

pub fn router(manager: Arc<Manager>) -> Router {
    Router::new()
        .route("/v1/branches", get(list).post(create))
        .route("/v1/branches/{name}", delete(remove))
        .route("/v1/branches/{name}/stop", post(stop))
        .route("/v1/gc", post(gc))
        .route("/v1/status", get(status))
        .with_state(manager)
}

async fn list(State(m): State<Arc<Manager>>) -> impl IntoResponse {
    Json(m.branches())
}

async fn create(State(m): State<Arc<Manager>>, Json(req): Json<CreateBranch>) -> Result<impl IntoResponse, Failure> {
    Ok((StatusCode::CREATED, Json(m.create_branch(&req).await?)))
}

async fn remove(State(m): State<Arc<Manager>>, Path(name): Path<String>, Query(q): Query<Force>) -> Result<impl IntoResponse, Failure> {
    m.delete_branch(&name, q.force).await?;
    Ok(Json(serde_json::json!({ "deleted": name })))
}

async fn stop(State(m): State<Arc<Manager>>, Path(name): Path<String>, Query(q): Query<Force>) -> Result<impl IntoResponse, Failure> {
    let stopped = m.stop(&name, q.force).await?;
    Ok(Json(serde_json::json!({ "branch": name, "stopped": stopped })))
}

async fn gc(State(m): State<Arc<Manager>>) -> Result<impl IntoResponse, Failure> {
    Ok(Json(m.gc().await?))
}

async fn status(State(m): State<Arc<Manager>>) -> impl IntoResponse {
    Json(m.status())
}
