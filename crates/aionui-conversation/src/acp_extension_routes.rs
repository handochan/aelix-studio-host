#![allow(clippy::disallowed_types)]

use aionui_ai_agent::protocol::acp_relay::{RelayError, valid_json_bounds, valid_method};
use aionui_api_types::{AcpExtensionRequest, ApiResponse};
use aionui_common::{ApiError, WsSessionGuard};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, FromRequest, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};

use crate::ConversationError;
use crate::acp_extension::ExtensionError;
use crate::state::ConversationRouterState;

const BODY_LIMIT: usize = 64 * 1024;

/// Created only by the protected Studio admission middleware.
#[derive(Clone)]
pub struct AcpExtensionAdmission {
    pub owner: String,
    pub browser_session: Option<WsSessionGuard>,
}

pub fn acp_extension_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/conversations/{id}/acp-extensions", get(discover).post(call))
        .with_state(state)
}

async fn discover(
    State(state): State<ConversationRouterState>,
    Path(conversation): Path<String>,
    admission: Option<Extension<AcpExtensionAdmission>>,
) -> Response {
    let result = async {
        let Extension(access) = admission.ok_or_else(auth_required)?;
        let data = state
            .service
            .discover_acp_extensions(&access.owner, &conversation)
            .await
            .map_err(extension_error)?;
        Ok::<_, Response>(Json(ApiResponse::ok(data)).into_response())
    }
    .await;
    let mut response = result.unwrap_or_else(|error| error);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

async fn call(
    State(state): State<ConversationRouterState>,
    Path(conversation): Path<String>,
    admission: Option<Extension<AcpExtensionAdmission>>,
    mut request: Request,
) -> Result<Response, Response> {
    let Extension(access) = admission.ok_or_else(auth_required)?;
    state
        .service
        .charge_acp_extension(&access.owner)
        .map_err(extension_error)?;
    let mut content_types = request.headers().get_all(header::CONTENT_TYPE).iter();
    let json = content_types
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    if !json || content_types.next().is_some() || request.headers().contains_key(header::CONTENT_ENCODING) {
        return Err(coded(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "EXT_MEDIA_TYPE",
            "Expected uncompressed application/json",
        ));
    }
    DefaultBodyLimit::max(BODY_LIMIT).apply(&mut request);
    let bytes = Bytes::from_request(request, &()).await.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            coded(
                StatusCode::PAYLOAD_TOO_LARGE,
                "EXT_TOO_LARGE",
                "ACP extension body exceeds 64 KiB",
            )
        } else {
            invalid_request()
        }
    })?;
    if !valid_json_bounds(&bytes) {
        return Err(invalid_request());
    }
    let request: AcpExtensionRequest = serde_json::from_slice(&bytes).map_err(|_| invalid_request())?;
    if request.binding_id.is_empty() || request.binding_id.len() > 96 {
        return Err(invalid_request());
    }
    if !valid_method(&request.method) {
        return Err(extension_error(RelayError::MethodDenied.into()));
    }
    let data = state
        .service
        .call_acp_extension(&access.owner, &conversation, request, access.browser_session.as_ref())
        .await
        .map_err(extension_error)?;
    Ok(Json(ApiResponse::ok(data)).into_response())
}

fn auth_required() -> Response {
    coded(
        StatusCode::UNAUTHORIZED,
        "AUTH_REQUIRED",
        "An active Studio session is required",
    )
}

fn invalid_request() -> Response {
    coded(StatusCode::BAD_REQUEST, "EXT_INVALID", "Invalid ACP extension request")
}

fn coded(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    ApiError::coded(status, code, message, None).into_response()
}

fn extension_error(error: ExtensionError) -> Response {
    match error {
        ExtensionError::Authentication => auth_required(),
        ExtensionError::Conversation(ConversationError::NotFound { .. }) => coded(
            StatusCode::NOT_FOUND,
            "CONVERSATION_NOT_FOUND",
            "Conversation not found",
        ),
        ExtensionError::Conversation(_) => coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "Could not check conversation ownership",
        ),
        ExtensionError::Relay(error) => match error {
            RelayError::AuthorizationRevoked => auth_required(),
            RelayError::NoLiveTask => coded(StatusCode::CONFLICT, "EXT_NO_LIVE_TASK", "No live ACP session"),
            RelayError::Unavailable => coded(
                StatusCode::CONFLICT,
                "EXT_UNAVAILABLE",
                "ACP extensions are unavailable",
            ),
            RelayError::StaleBinding => coded(
                StatusCode::CONFLICT,
                "EXT_STALE_BINDING",
                "ACP extension binding changed",
            ),
            RelayError::MethodDenied => coded(
                StatusCode::FORBIDDEN,
                "EXT_METHOD_DENIED",
                "ACP extension method denied",
            ),
            RelayError::RateLimited => {
                let mut response = coded(
                    StatusCode::TOO_MANY_REQUESTS,
                    "EXT_RATE_LIMITED",
                    "ACP extension limit reached",
                );
                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
                response
            }
            RelayError::Agent(code) => ApiError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "EXT_AGENT_ERROR",
                "ACP agent rejected the extension request",
                Some(serde_json::json!({ "rpcCode": code })),
            )
            .into_response(),
            RelayError::TransportUnknown | RelayError::BindingUnknown | RelayError::DeadlineUnknown => {
                let status = match error {
                    RelayError::DeadlineUnknown => StatusCode::GATEWAY_TIMEOUT,
                    RelayError::BindingUnknown => StatusCode::CONFLICT,
                    _ => StatusCode::BAD_GATEWAY,
                };
                ApiError::coded(
                    status,
                    "EXT_OUTCOME_UNKNOWN",
                    "ACP extension outcome is unknown",
                    Some(serde_json::json!({ "retryable": false })),
                )
                .into_response()
            }
        },
    }
}
