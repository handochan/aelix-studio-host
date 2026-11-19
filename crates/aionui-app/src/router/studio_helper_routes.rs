use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{Method, header};
use serde::{Deserialize, Serialize};

pub(super) enum HelperGrant {
    Route,
    Rename,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConversationRename {
    name: String,
}

/// A rename uses the general PATCH handler, so forward only the admitted field.
pub(super) async fn rename_request(request: Request) -> Option<Request> {
    const MAX_RENAME_BYTES: usize = 16 * 1024;

    let (mut parts, body) = request.into_parts();
    let content_type = parts.headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    if !content_type
        .split(';')
        .next()?
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return None;
    }

    let bytes = to_bytes(body, MAX_RENAME_BYTES).await.ok()?;
    let rename: ConversationRename = serde_json::from_slice(&bytes).ok()?;
    let body = Body::from(serde_json::to_vec(&rename).ok()?);
    parts.headers.remove(header::CONTENT_LENGTH);
    Some(Request::from_parts(parts, body))
}

/// Match the routes used by the config and diagnose CLIs. Runtime tools
/// with their own capabilities have a separate admission path.
pub(super) fn grant(method: &Method, path: &str, conversation: &str) -> Option<HelperGrant> {
    if *method == Method::GET && path == "/health" {
        return Some(HelperGrant::Route);
    }

    let parts: Vec<_> = path.split('/').collect();
    let ["", "api", rest @ ..] = parts.as_slice() else {
        return None;
    };

    if let (&Method::PATCH, ["conversations", id]) = (method, rest) {
        return (*id == conversation).then_some(HelperGrant::Rename);
    }

    let allowed = match (method, rest) {
        (&Method::GET, ["system", "info"] | ["conversations"] | ["teams"]) => true,
        (&Method::GET, ["conversations", id] | ["conversations", id, "messages"]) => *id == conversation,

        (&Method::GET | &Method::POST, ["assistants" | "providers"] | ["mcp", "servers"] | ["cron", "jobs"]) => true,
        (
            &Method::GET | &Method::PUT | &Method::DELETE,
            ["assistants" | "providers", _] | ["mcp", "servers", _] | ["cron", "jobs", _],
        ) => true,
        (&Method::PATCH, ["assistants", _, "state"]) => true,
        (
            &Method::POST,
            ["assistants", "import"] | ["providers", "detect-protocol" | "fetch-models"] | ["providers", _, "models"],
        ) => true,

        (&Method::GET, ["skills"] | ["skills", "paths" | "external-paths"]) => true,
        (
            &Method::POST,
            ["skills", "info" | "import" | "scan" | "external-paths"]
            | ["skills", "market", "enable" | "disable"]
            | ["skills", "assistant-rule" | "assistant-skill", "read" | "write"],
        ) => true,
        (&Method::DELETE, ["skills", _] | ["skills", "assistant-rule" | "assistant-skill", _]) => true,

        (&Method::GET, ["mcp", "agent-configs"] | ["mcp", "oauth", "authenticated"]) => true,
        (
            &Method::POST,
            ["mcp", "test-connection"]
            | ["mcp", "servers", "import"]
            | ["mcp", "servers", _, "toggle"]
            | ["mcp", "oauth", "check-status" | "login" | "logout"],
        ) => true,
        (&Method::GET | &Method::PATCH, ["settings"]) | (&Method::GET | &Method::PUT, ["settings", "client"]) => true,

        (&Method::GET, ["agents"] | ["agents", "management"]) => true,
        (&Method::POST, ["agents", "provider-health-check" | "custom"] | ["agents", "custom", "try-connect"]) => true,
        (&Method::PATCH, ["agents", _, "enabled"])
        | (&Method::PUT, ["agents", _, "overrides"])
        | (&Method::PUT | &Method::DELETE, ["agents", "custom", _]) => true,

        (&Method::GET | &Method::POST | &Method::DELETE, ["cron", "jobs", _, "skill"])
        | (&Method::POST, ["cron", "jobs", _, "run"])
        | (&Method::GET, ["internal", "conversation-cron", "list"])
        | (&Method::POST, ["internal", "conversation-cron", "create"])
        | (&Method::PUT, ["internal", "conversation-cron", "jobs", _]) => true,
        _ => false,
    };
    allowed.then_some(HelperGrant::Route)
}
