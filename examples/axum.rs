//! El caso de pagos con axum: `GET /payment-methods` lista los métodos con su estado, y
//! `POST /payments` y `POST /refunds` exigen el método y la operación antes de ejecutarla.
//!
//! ```text
//! cargo run --example axum
//! curl localhost:3000/payment-methods
//! curl -X POST localhost:3000/refunds -H 'content-type: application/json' -d '{"method":"paypal"}'
//! ```
//!
//! Con el servidor en marcha, edita `examples/flags.toml`: el log muestra la recarga (o por qué
//! se rechazó) y la siguiente petición ya ve el cambio.

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

// Keys fijas: si faltan en el archivo, el servidor no arranca.
flag_key!(CHARGE = "payments.ops.charge");
flag_key!(REFUND = "payments.ops.refund");

type AppFlags = Arc<Flags<Meta>>;

/// Lo que cada entrada del archivo trae en `meta`.
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
    // Vive lo que `main`: soltarlo pararía la recarga.
    let (flags, watcher) = Flags::<Meta>::watch_file(path)?;
    flags.on_change(|diff| tracing::info!(?diff, "cambio aplicado"));
    // Con el target de la app: un filtro por crate no lo esconde. Si solo quedara en el log de
    // la librería, un archivo roto dejaría el servicio con el estado viejo sin que nadie lo viera.
    // Sin "sigue el anterior": también llega la vigilancia perdida, que no es un rechazo.
    watcher.on_reject(|e| tracing::error!("flags.toml: {e:#}"));

    let app = Router::new()
        .route("/payment-methods", get(payment_methods))
        .route("/payments", post(charge))
        .route("/refunds", post(refund))
        .with_state(flags);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("escuchando en http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Informativo: entre réplicas, o entre este GET y el POST, el estado puede cambiar. La
/// autoridad es el `require` del POST.
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
    // Aquí iría el cobro.
    Ok(StatusCode::ACCEPTED)
}

async fn refund(
    State(flags): State<AppFlags>,
    Json(req): Json<OperationRequest>,
) -> Result<StatusCode, ApiError> {
    let m = segment(&req.method)?;
    // Un `require` por dimensión: el método con su `.refund` (cascada) y los reembolsos en global.
    require_method(&flags, &format!("payments.methods.{m}.refund"))?;
    flags.require(REFUND)?;
    // Aquí iría el reembolso.
    Ok(StatusCode::ACCEPTED)
}

/// `require` sobre una key armada con input del usuario: si no existe, lo que no existe es el
/// método, y es un 400. Sobre una key fija sería un bug de configuración (ver `From`).
fn require_method(flags: &Flags<Meta>, key: &str) -> Result<(), ApiError> {
    flags.require(key).map_err(|e| match e {
        FlagError::Unknown { .. } => ApiError::new(StatusCode::BAD_REQUEST, "método desconocido"),
        e => e.into(),
    })
}

/// La traducción a HTTP es de la app, no de la librería.
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
            // El `reason` está escrito para el usuario final.
            FlagError::Disabled { reason, .. } => {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, reason)
            }
            FlagError::InvalidSegment { .. } => {
                ApiError::new(StatusCode::BAD_REQUEST, "método inválido")
            }
            // `Unknown` sobre una key fija, o una variante futura (`FlagError` es non_exhaustive).
            e => {
                tracing::error!(error = %e, "flags mal configurados");
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "error interno")
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
