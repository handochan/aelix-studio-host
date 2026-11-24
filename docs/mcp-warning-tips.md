# MCP attachment warning contract

An MCP attachment warning is a non-terminal conversation tip with `code` set
to `MCP_SERVER_UNAVAILABLE` and `type` set to `warning`. The tip object is at
`data.data` in the `message.stream` WebSocket event and at `content` in a
saved `tips` message.

| Host error | `params.reason_code` | Meaning |
| --- | --- | --- |
| `McpResolveError::ConfigurationUnavailable` | `configuration_unavailable` | The host could not load the MCP settings. |
| `McpResolveError::InvalidConfiguration` | `invalid_configuration` | An MCP transport configuration is invalid. |
| `McpResolveError::Runtime` | `runtime_unavailable` | The configured MCP command or runtime could not be prepared. |

The host uses `invalid_configuration` when it rejects a stored MCP transport
configuration or a transport configuration from a session's MCP server snapshot.
When snapshot conversion in the ACP or aionrs factory rejects an empty stdio
command or an empty HTTP, Streamable HTTP, or SSE URL, the tip carries the
snapshot entry's `server_id` and `server_name` with this reason code.

Use `params.reason_code` to select the localized message in Studio. The strings
in the table are stable. For older tips without a reason code or tips with an
unrecognized code, display the supplied English `content` verbatim.

`params.server_id` is the existing server identifier. `params.server_name` is
the display name already used in `content` and can be interpolated as text
in a server-specific translation. When `reason_code` is
`configuration_unavailable`, `server_id` is empty and `server_name` contains
the English label `Configured MCP servers`. Use a translation with no server
name placeholder for this code. The tip retains its `supersedes_key` in the
form `mcp-unavailable:<server_id>` for grouping repeated warnings.

The common emitter builds the tip from the identifier, display name, and
error variant. Its payload excludes stored transport details and raw error
text. The resolver tests cover the serialized payload for all three variants
and verify that the `runtime_unavailable` parameters survive a history reload.
See [the host resolver and contract tests](../crates/aionui-ai-agent/src/mcp_resolve.rs).
