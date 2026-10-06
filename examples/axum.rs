//! The payments case with axum: `GET /payment-methods` lists the methods with their state, and
//! `POST /payments` and `POST /refunds` require the method and the operation before running it.
//!
//! ```text
//! cargo run --example axum
//! curl localhost:3000/payment-methods
//! curl -X POST localhost:3000/refunds -H 'content-type: application/json' -d '{"method":"paypal"}'
//! ```
//!
//! With the server running, edit `examples/flags.toml`: the log shows the reload (or why it was
//! rejected) and the next request already sees the change.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use breaker_panel::{FlagError, Flags, flag_key, segment};
use serde::{Deserialize, Serialize};

// Fixed keys: if they are missing from the file, the server does not start.
flag_key!(CHARGE = "payments.ops.charge");
flag_key!(REFUND = "payments.ops.refund");

type AppFlags = Arc<Flags<Meta>>;

/// What each file entry has in `meta`.
#[derive(Debug, Default, Deserialize)]
struct Meta {
    display_name: String,
}

#[derive(Serialize)]
struct MethodDto {
    id: String,
    name: String,
    enabled: bool,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct OperationRequest {
    method: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/flags.toml");
    // Lives as long as `main`: dropping it would stop reloading.
    let (flags, watcher) = Flags::<Meta>::watch_file(path)?;
    flags.on_change(|diff| tracing::info!(?diff, "change applied"));
    // With the app's target: a per-crate filter does not hide it. If it only reached the
    // library's log, a broken file would leave the service on the old state without anyone
    // noticing. No "keeping the previous one": the lost watch also arrives, and it is not a
    // rejection.
    watcher.on_reject(|e| tracing::error!("flags.toml: {e:#}"));

    let app = Router::new()
        .route("/payment-methods", get(payment_methods))
        .route("/payments", post(charge))
        .route("/refunds", post(refund))
        .with_state(flags);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Informational: across replicas, or between this GET and the POST, the state may change. The
/// authority is the POST's `require`.
async fn payment_methods(State(flags): State<AppFlags>) -> Json<Vec<MethodDto>> {
    let snap = flags.snapshot();
    let methods = snap.children("payments.methods").map(|(key, r)| MethodDto {
        id: key.rsplit_once('.').map_or(key, |(_, id)| id).to_owned(),
        name: r.meta.display_name.clone(),
        enabled: r.enabled,
        reason: r.reason.clone(),
    });
    Json(methods.collect())
}

async fn charge(
    State(flags): State<AppFlags>,
    Json(req): Json<OperationRequest>,
) -> Result<StatusCode, ApiError> {
    let m = segment(&req.method)?;
    require_method(&flags, &format!("payments.methods.{m}"))?;
    flags.require(CHARGE)?;
    // The charge would go here.
    Ok(StatusCode::ACCEPTED)
}

async fn refund(
    State(flags): State<AppFlags>,
    Json(req): Json<OperationRequest>,
) -> Result<StatusCode, ApiError> {
    let m = segment(&req.method)?;
    // One `require` per dimension: the method with its `.refund` (cascade) and refunds globally.
    require_method(&flags, &format!("payments.methods.{m}.refund"))?;
    flags.require(REFUND)?;
    // The refund would go here.
    Ok(StatusCode::ACCEPTED)
}

/// `require` on a key built from user input: if it does not exist, what does not exist is the
/// method, and that is a 400. On a fixed key it would be a configuration bug (see `From`).
fn require_method(flags: &Flags<Meta>, key: &str) -> Result<(), ApiError> {
    flags.require(key).map_err(|e| match e {
        FlagError::Unknown { .. } => ApiError::new(StatusCode::BAD_REQUEST, "unknown method"),
        e => e.into(),
    })
}

/// Mapping to HTTP belongs to the app, not to the library.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        let message = message.into();
        Self { status, message }
    }
}

impl From<FlagError> for ApiError {
    fn from(e: FlagError) -> Self {
        match e {
            // The `reason` is written for the end user.
            FlagError::Disabled { reason, .. } => {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, reason)
            }
            FlagError::InvalidSegment { .. } => {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid method")
            }
            // `Unknown` on a fixed key, or a future variant (`FlagError` is non_exhaustive).
            e => {
                tracing::error!(error = %e, "misconfigured flags");
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        let body = Body {
            error: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}
