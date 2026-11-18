use axum::http::Method;

/// Match the routes used by the config and diagnose CLIs. Runtime tools
/// with their own capabilities have a separate admission path.
pub(super) fn allows(method: &Method, path: &str, conversation: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    let ["", "api", rest @ ..] = parts.as_slice() else {
        return false;
    };

    match (method, rest) {
        (&Method::GET, ["system", "info"] | ["conversations"] | ["teams"]) => true,
        (&Method::GET, ["conversations", id] | ["conversations", id, "messages"])
        | (&Method::PATCH, ["conversations", id]) => *id == conversation,

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
    }
}
