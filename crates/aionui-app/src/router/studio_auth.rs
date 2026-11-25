#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_auth::{
    JwtService, RUNTIME_CONVERSATION_ID_HEADER, RUNTIME_TOKEN_HEADER, RUNTIME_USER_ID_HEADER,
    extract_token_from_headers,
};
use aionui_common::ApiError;
use aionui_conversation::AcpExtensionAdmission;
use aionui_db::IUserRepository;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;

use crate::AppServices;

const DESKTOP_HEADER: &str = "x-aionui-studio-auth";
const OPERATOR: &str = "system_default_user";

/// Admission for the Studio-owned loopback listener. The launch credential
/// lives in the supervisor and host and is consumed before routing.
pub struct StudioAuth {
    authority: String,
    origin: String,
    secret: [u8; 64],
}

impl StudioAuth {
    pub(crate) fn operator_id(&self) -> &'static str {
        OPERATOR
    }

    pub fn new(port: u16, secret: [u8; 64]) -> Self {
        let authority = format!("127.0.0.1:{port}");
        Self {
            origin: format!("http://{authority}"),
            authority,
            secret,
        }
    }

    pub(super) fn protect(self, router: Router, services: &AppServices) -> Router {
        let state = AdmissionState {
            auth: self,
            jwt: services.jwt_service.clone(),
            users: services.user_repo.clone(),
            runtime_tokens: services.runtime_token_service.clone(),
            cookies: services.cookie_config.clone(),
        };
        router.layer(middleware::from_fn_with_state(Arc::new(state), admit))
    }

    fn trusted(&self, headers: &HeaderMap) -> bool {
        single_header(headers, DESKTOP_HEADER)
            .and_then(|value| <&[u8; 64]>::try_from(value.as_bytes()).ok())
            .is_some_and(|supplied| {
                self.secret
                    .iter()
                    .zip(supplied)
                    .fold(0, |difference, (left, right)| difference | (left ^ right))
                    == 0
            })
    }

    fn accepts_browser_source(&self, headers: &HeaderMap, method: &Method, websocket: bool) -> bool {
        if headers.contains_key("sec-fetch-site")
            && !matches!(single_header(headers, "sec-fetch-site"), Some("none" | "same-origin"))
        {
            return false;
        }
        if headers.contains_key(header::ORIGIN) {
            return single_header(headers, "origin") == Some(self.origin.as_str());
        }
        !websocket && matches!(*method, Method::GET | Method::HEAD)
    }
}

struct AdmissionState {
    auth: StudioAuth,
    jwt: Arc<JwtService>,
    users: Arc<dyn IUserRepository>,
    runtime_tokens: Arc<RuntimeTokenService>,
    cookies: Arc<aionui_auth::CookieConfig>,
}

async fn admit(
    State(state): State<Arc<AdmissionState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if single_header(request.headers(), "host") != Some(state.auth.authority.as_str()) {
        return Err(forbidden());
    }

    let desktop = state.auth.trusted(request.headers());
    if request.headers_mut().remove(DESKTOP_HEADER).is_some() && !desktop {
        return Err(unauthenticated());
    }

    let path = request.uri().path();
    let acp_relay = path
        .strip_prefix("/api/conversations/")
        .and_then(|path| {
            path.strip_suffix("/acp-extensions")
                .or_else(|| path.strip_suffix("/byte-grants"))
        })
        .is_some_and(|id| !id.is_empty() && !id.contains('/'));
    if acp_relay
        && (request.headers().contains_key(RUNTIME_TOKEN_HEADER)
            || request.headers().contains_key(RUNTIME_USER_ID_HEADER)
            || request.headers().contains_key(RUNTIME_CONVERSATION_ID_HEADER)
            || request.headers().contains_key("x-aioncore-bootstrap-secret")
            || request.headers().contains_key("x-aionui-hook-token"))
    {
        return Err(forbidden());
    }
    let realtime = path == "/ws";
    let websocket = realtime || request.headers().contains_key(header::UPGRADE);
    if desktop {
        let headers = request.headers();
        if (headers.contains_key(header::ORIGIN) || websocket || request.method() == Method::OPTIONS)
            && single_header(headers, "origin") != Some(state.auth.origin.as_str())
        {
            return Err(forbidden());
        }
        if acp_relay {
            if headers.contains_key(header::AUTHORIZATION) || extract_token_from_headers(headers).is_some() {
                return Err(forbidden());
            }
            request.extensions_mut().insert(AcpExtensionAdmission {
                owner: OPERATOR.into(),
                browser_session: None,
            });
        }
        if realtime {
            let user = state
                .users
                .find_active_by_id(OPERATOR)
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(unauthenticated)?;
            let token = state
                .jwt
                .sign_with_session_generation(&user.id, "studio:desktop", user.session_generation)
                .map_err(|_| unavailable())?;
            let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| unavailable())?;
            value.set_sensitive(true);
            request.headers_mut().insert(header::AUTHORIZATION, value);
        }
        return Ok(next.run(request).await);
    }

    if is_desktop_route(path) {
        return Err(forbidden());
    }

    let headers = request.headers();
    let native = !headers.contains_key(header::ORIGIN) && !headers.contains_key("sec-fetch-site");
    if requires_runtime_guard(request.method(), path) {
        if !native || websocket {
            return Err(forbidden());
        }
        // These routes use their existing method-specific capability verification.
        return Ok(next.run(request).await);
    }
    if headers.contains_key(RUNTIME_TOKEN_HEADER) {
        if !native || websocket {
            return Err(forbidden());
        }

        let grant = state
            .helper_grant(headers, request.method(), path)
            .ok_or_else(forbidden)?;
        let request = match grant {
            super::studio_helper_routes::HelperGrant::Route => request,
            super::studio_helper_routes::HelperGrant::Rename => super::studio_helper_routes::rename_request(request)
                .await
                .ok_or_else(forbidden)?,
        };
        return Ok(next.run(request).await);
    }
    if path.starts_with("/api/internal/") || path.starts_with("/api/runtime/") || path.starts_with("/internal/") {
        return Err(forbidden());
    }

    let method = request.method();
    if !state.auth.accepts_browser_source(headers, method, websocket) {
        return Err(forbidden());
    }

    if !websocket && is_public(method, path) {
        return Ok(next.run(request).await);
    }

    let token = extract_token_from_headers(headers).ok_or_else(unauthenticated)?;
    let claims = state.jwt.verify_access(&token).map_err(|_| unauthenticated())?;
    if claims.user_id != OPERATOR || claims.exp <= (aionui_common::now_ms() / 1000) as u64 {
        return Err(unauthenticated());
    }
    let user = state
        .users
        .find_active_by_id(OPERATOR)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthenticated)?;
    if claims.session_generation != user.session_generation {
        return Err(unauthenticated());
    }
    if acp_relay || matches!(path, "/ws" | "/api/stt/stream") {
        let jwt = Arc::clone(&state.jwt);
        let users = Arc::clone(&state.users);
        let expires = claims.exp;
        let generation = claims.session_generation;
        let guard = aionui_common::WsSessionGuard::new(
            move || expires > (aionui_common::now_ms() / 1000) as u64 && jwt.verify_access(&token).is_ok(),
            move || {
                let users = Arc::clone(&users);
                async move {
                    matches!(users.find_active_by_id(OPERATOR).await,
                        Ok(Some(user)) if user.session_generation == generation)
                }
            },
        );
        if acp_relay {
            let jwt = state.jwt.clone();
            let guard = guard.with_revocation_fence(move |write| jwt.with_revocation_fence(write));
            request.extensions_mut().insert(AcpExtensionAdmission {
                owner: OPERATOR.into(),
                browser_session: Some((guard, generation)),
            });
            return aionui_auth::csrf_middleware(State(state.cookies.clone()), request, next).await;
        }
        request.extensions_mut().insert(guard);
    }
    Ok(next.run(request).await)
}

impl AdmissionState {
    fn helper_grant(
        &self,
        headers: &HeaderMap,
        method: &Method,
        path: &str,
    ) -> Option<super::studio_helper_routes::HelperGrant> {
        let (Some(token), Some(OPERATOR), Some(conversation)) = (
            single_header(headers, RUNTIME_TOKEN_HEADER),
            single_header(headers, RUNTIME_USER_ID_HEADER),
            single_header(headers, RUNTIME_CONVERSATION_ID_HEADER),
        ) else {
            return None;
        };
        let grant = super::studio_helper_routes::grant(method, path, conversation)?;
        self.runtime_tokens
            .validate(
                Some(token),
                OPERATOR,
                conversation,
                RuntimeTokenScope::ConversationHelper,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .ok()?;
        Some(grant)
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn is_public(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET | Method::HEAD => matches!(path, "/health" | "/qr-login"),
        Method::POST => matches!(path, "/login" | "/api/auth/qr-login" | "/api/auth/refresh"),
        _ => false,
    }
}

fn is_desktop_route(path: &str) -> bool {
    path.starts_with("/api/auth/internal/")
        || path.starts_with("/api/webui/")
        || path == "/api/cron/internal/system-resume"
}

fn requires_runtime_guard(method: &Method, path: &str) -> bool {
    match *method {
        Method::GET => {
            matches!(
                path,
                "/api/runtime/team-tools/context" | "/api/runtime/session-messages/targets"
            ) || path == "/api/runtime/skills"
                || path.starts_with("/api/runtime/skills/")
        }
        Method::POST => {
            matches!(
                path,
                "/api/runtime/team-tools/call"
                    | "/api/runtime/session-messages/send"
                    | "/api/runtime/conversations/create"
            ) || path.starts_with("/internal/antigravity-hook/")
        }
        _ => false,
    }
}

fn unauthenticated() -> ApiError {
    ApiError::coded(
        StatusCode::UNAUTHORIZED,
        "AUTH_REQUIRED",
        "Authentication required.",
        None,
    )
}

fn forbidden() -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
        "Request source forbidden.",
        None,
    )
}

fn unavailable() -> ApiError {
    ApiError::Internal("Authentication service unavailable".into())
}
