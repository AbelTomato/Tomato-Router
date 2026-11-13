use std::collections::HashMap;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{Value, json};
use tower_http::trace::TraceLayer;

use crate::error::GatewayError;
use crate::gateway;
use crate::state::AppState;

pub fn build_app(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/responses", post(responses))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{model}", get(get_model))
        .route("/v1/responses/{response_id}", get(get_response))
        .route("/admin/status", get(admin_status))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(DefaultBodyLimit::max(state.config.server.max_body_bytes));

    Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(protected)
        .fallback(not_found)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    if body.is_empty() {
        return Err(GatewayError::bad_request("request body is empty"));
    }
    let request: Value = serde_json::from_slice(&body)
        .map_err(|error| GatewayError::bad_request(format!("invalid JSON body: {error}")))?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .ok_or_else(|| GatewayError::bad_request("missing `model`"))?;
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    tracing::info!(
        model,
        stream,
        bytes = body.len(),
        "received Responses request"
    );
    if state.config.server.log_request_bodies {
        let preview: String = String::from_utf8_lossy(&body)
            .chars()
            .take(64_000)
            .collect();
        tracing::info!(body = %preview, "request body");
    }

    gateway::execute(&state, request, &model, stream, &headers).await
}

async fn list_models(State(state): State<AppState>) -> Json<Value> {
    let created = state.started_at_unix();
    let data: Vec<Value> = state
        .router
        .model_ids()
        .into_iter()
        .map(|id| model_json(&id, created))
        .collect();
    Json(json!({
        "object": "list",
        "data": data,
    }))
}

async fn get_model(
    State(state): State<AppState>,
    Path(model): Path<String>,
) -> Result<Json<Value>, GatewayError> {
    if state.router.model_ids().iter().any(|id| id == &model) {
        Ok(Json(model_json(&model, state.started_at_unix())))
    } else {
        Err(GatewayError::not_found(format!(
            "model `{model}` is not configured"
        )))
    }
}

async fn get_response(Path(response_id): Path<String>) -> GatewayError {
    GatewayError::not_found(format!(
        "response `{response_id}` is not stored by this stateless gateway; use `store: false` \
         and send full input history"
    ))
}

async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "tomato-router",
        "version": env!("CARGO_PKG_VERSION"),
        "providers": state.providers.all().len(),
        "uptime_seconds": state.uptime_seconds(),
    }))
}

async fn readyz(State(state): State<AppState>) -> Response {
    let available = state
        .providers
        .all()
        .iter()
        .filter(|provider| provider.breaker_available())
        .count();
    let status = if available > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if available > 0 { "ready" } else { "unavailable" },
            "available_providers": available,
            "total_providers": state.providers.all().len(),
        })),
    )
        .into_response()
}

async fn admin_status(State(state): State<AppState>) -> Json<Value> {
    let auth: HashMap<String, (bool, String)> = state
        .config
        .provider_env_status()
        .into_iter()
        .map(|(id, ok, detail)| (id, (ok, detail)))
        .collect();

    let providers: Vec<Value> = state
        .providers
        .all()
        .iter()
        .map(|provider| {
            let mut status = provider.status();
            if let Some((ok, detail)) = auth.get(&provider.config.id) {
                status["auth"] = json!({
                    "ok": ok,
                    "detail": detail,
                });
            }
            status
        })
        .collect();

    let routes: Vec<Value> = state
        .config
        .routes
        .iter()
        .map(|route| {
            json!({
                "model": route.model,
                "chain": route.chain,
            })
        })
        .collect();

    Json(json!({
        "service": "tomato-router",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.uptime_seconds(),
        "listen": state.config.server.listen,
        "require_auth": state.config.server.require_auth,
        "providers": providers,
        "routes": routes,
    }))
}

async fn index(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "service": "tomato-router",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Strict-priority OpenAI Responses gateway",
        "providers": state.providers.all().len(),
        "endpoints": {
            "responses": "POST /v1/responses",
            "models": "GET /v1/models",
            "health": "GET /healthz",
            "readiness": "GET /readyz",
            "status": "GET /admin/status",
        },
    }))
}

async fn not_found(method: axum::http::Method, uri: Uri) -> GatewayError {
    GatewayError::not_found(format!("no route for {method} {uri}"))
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !state.config.server.require_auth {
        return next.run(request).await;
    }

    let provided = provided_token(request.headers());
    let authorized = provided
        .map(|token| {
            state
                .api_keys
                .iter()
                .any(|key| constant_time_eq(key.as_bytes(), token.as_bytes()))
        })
        .unwrap_or(false);

    if authorized {
        next.run(request).await
    } else {
        let mut response = GatewayError::unauthorized().into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
        response
    }
}

fn provided_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        && let Some((scheme, token)) = value.trim().split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        return Some(token.trim());
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn model_json(id: &str, created: u64) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": "tomato-router",
    })
}
