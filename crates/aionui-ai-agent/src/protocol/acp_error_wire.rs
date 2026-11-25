//! Public application error validation at the ACP stdio boundary.

use std::{borrow::Cow, collections::BTreeMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};

use super::error::{
    ACP_REQUEST_FAILED, MODEL_SELECTION_REQUIRED, TURN_FAILED_AFTER_DISPATCH, safe_provider_cause, turn_failure_data,
};

#[derive(Deserialize, Serialize)]
struct WireReply<'a> {
    jsonrpc: &'a RawValue,
    id: &'a RawValue,
    #[serde(borrow)]
    error: WireError<'a>,
}

#[derive(Deserialize, Serialize)]
struct WireError<'a> {
    code: i32,
    message: &'a RawValue,
    #[serde(borrow, skip_serializing_if = "Option::is_none")]
    data: Option<&'a RawValue>,
}

/// aelix-acp@a6545f0b/src/aelix_acp/wire/errors.py defines the refusal.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelSelectionRefusal {
    kind: String,
    config_id: String,
    #[serde(rename = "availableModels")]
    _available_models: u32,
    retryable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnFailure<'a> {
    kind: String,
    retryable: bool,
    #[serde(borrow)]
    cause: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderCause {
    kind: String,
    retryable: bool,
}

fn wire_reply(raw: &str, code: i32) -> Option<WireReply<'_>> {
    let reply: WireReply<'_> = serde_json::from_str(raw).ok()?;
    (reply.error.code == code).then_some(reply)
}

fn valid_model_selection_refusal(raw: &str) -> bool {
    wire_reply(raw, -32004)
        .and_then(|reply| serde_json::from_str::<ModelSelectionRefusal>(reply.error.data?.get()).ok())
        .is_some_and(|data| data.kind == MODEL_SELECTION_REQUIRED && data.config_id == "model" && !data.retryable)
}

fn wire_provider_cause(raw: &str) -> Option<Value> {
    let data = wire_reply(raw, -33002)?.error.data?.get();
    if !data.trim_start().starts_with('{') {
        return None;
    }
    let failure: TurnFailure<'_> = serde_json::from_str(data).ok()?;
    let cause = failure.cause.get();
    // Derived structs also accept positional arrays, which are outside this contract.
    if failure.kind != TURN_FAILED_AFTER_DISPATCH || failure.retryable || !cause.trim_start().starts_with('{') {
        return None;
    }
    let cause: ProviderCause = serde_json::from_str(cause).ok()?;
    safe_provider_cause(&cause.kind, cause.retryable)
}

fn sanitize_error(reply: &mut Value, raw: Option<&str>) -> bool {
    if !reply["error"]["message"].is_string() {
        return false;
    }
    let code = reply["error"]["code"].as_i64();
    let kind = reply["error"]["data"]["kind"].as_str();
    let data = match (code, kind) {
        (Some(-32004), Some(MODEL_SELECTION_REQUIRED)) => raw
            .is_some_and(valid_model_selection_refusal)
            .then(|| reply["error"]["data"].take()),
        (Some(-33002), Some(TURN_FAILED_AFTER_DISPATCH)) => {
            let data = turn_failure_data(raw.and_then(wire_provider_cause));
            if let Some(envelope) = reply.as_object_mut() {
                envelope.retain(|field, value| match field.as_str() {
                    "jsonrpc" | "id" | "error" => true,
                    // Keep malformed hybrids invalid without retaining their payloads.
                    "method" | "result" => {
                        *value = Value::Null;
                        true
                    }
                    _ => false,
                });
            }
            Some(data)
        }
        _ => return false,
    };
    let mut error = json!({"code": code, "message": ACP_REQUEST_FAILED});
    if let Some(data) = data {
        error["data"] = data;
    }
    reply["error"] = error;
    true
}

fn recover_undecodable_error(line: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Tag {
        kind: String,
    }

    // Match the SDK's response boundary. Projecting a hybrid or a
    // sequence-shaped error into a response would falsely confirm rejection.
    let envelope: BTreeMap<String, &RawValue> = serde_json::from_str(line).ok()?;
    if envelope.contains_key("method")
        || envelope.contains_key("result")
        || !envelope.get("error")?.get().trim_start().starts_with('{')
    {
        return None;
    }
    let mut reply: WireReply<'_> = serde_json::from_str(line).ok()?;
    // A response with a missing or non-string message is invalid even if the
    // error's code and kind can be read from the raw bytes.
    serde_json::from_str::<String>(reply.error.message.get()).ok()?;
    let data = reply.error.data?.get();
    if !data.trim_start().starts_with('{') {
        return None;
    }
    let tag: Tag = serde_json::from_str(data).ok()?;
    let data = match (reply.error.code, tag.kind.as_str()) {
        (-32004, MODEL_SELECTION_REQUIRED) => None,
        (-33002, TURN_FAILED_AFTER_DISPATCH) => Some(serde_json::value::to_raw_value(&turn_failure_data(None)).ok()?),
        _ => return None,
    };
    let message = serde_json::value::to_raw_value(ACP_REQUEST_FAILED).ok()?;
    reply.error.message = &message;
    reply.error.data = data.as_deref();
    serde_json::to_string(&reply).ok()
}

/// The SDK first reads lines as `Value`, which folds duplicate keys and can
/// coerce special objects into scalars. Use that view to identify anything
/// the SDK would see as one of these errors. Only the original JSON can validate it.
/// Replace peer text here so SDK and host diagnostics see the public detail.
pub(super) fn sanitize_public_errors(line: String, normalized: Option<Value>) -> String {
    if line.trim_start().starts_with('[')
        && let Ok(raw) = serde_json::from_str::<Vec<&RawValue>>(&line)
    {
        // Legacy stdio supports batches. Preserve the raw text of siblings
        // since reserializing a Value can change how the SDK parses them.
        let mut changed = false;
        let entries: Vec<_> = raw
            .into_iter()
            .map(|raw| {
                let raw = raw.get();
                let rewritten = match serde_json::from_str::<Value>(raw) {
                    Ok(mut reply) => sanitize_error(&mut reply, Some(raw)).then(|| reply.to_string()),
                    Err(_) => recover_undecodable_error(raw),
                };
                match rewritten {
                    Some(entry) => {
                        changed = true;
                        Cow::Owned(entry)
                    }
                    None => Cow::Borrowed(raw),
                }
            })
            .collect();
        if changed {
            return format!("[{}]", entries.join(","));
        }
        return line;
    }
    let Some(mut normalized) = normalized else {
        // A syntactically valid data object can also fail Value decoding.
        return recover_undecodable_error(&line).unwrap_or(line);
    };
    let changed = if let Some(entries) = normalized.as_array_mut() {
        // The normalized view can expose a batch synthesized from a raw
        // object. No entry has an original JSON object to validate against.
        let mut changed = false;
        for reply in entries {
            changed |= sanitize_error(reply, None);
        }
        changed
    } else {
        sanitize_error(&mut normalized, Some(&line))
    };
    if changed { normalized.to_string() } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_cause_requires_a_strict_raw_object() {
        for cause in [
            r#"{"kind":"provider_auth","retryable":true}"#,
            r#"{"kind":"provider_auth","retryable":"false"}"#,
            r#"{"kind":"provider_auth","retryable":null}"#,
            r#"{"kind":"provider_auth","retryable":0}"#,
            r#"{"kind":"provider_auth","retryable":false,"detail":"private peer data"}"#,
            r#"{"kind":"provider_auth","kind":"unknown","retryable":false}"#,
            r#"{"kind":"provider_auth","retryable":true,"retryable":false}"#,
            r#"{"kind":"provider_auth","retryable":{"$serde_json::private::RawValue":"false"}}"#,
            r#"{"kind":"provider_auth","retryable":{"$serde_json::private::RawValue":"private peer data"}}"#,
            r#"{"kind":"provider_auth","retryable":1e400}"#,
            r#"{"kind":"private peer data","retryable":false}"#,
            r#"{"kind":false,"retryable":false}"#,
            r#"{"kind":"provider_auth"}"#,
            r#"{"retryable":false}"#,
            r#"["provider_auth",false]"#,
            "null",
        ] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-33002,"message":"private peer text","data":{{"kind":"turn_failed_after_dispatch","retryable":false,"cause":{cause}}}}}}}"#
            );
            let normalized = serde_json::from_str(&line).ok();
            let output = sanitize_public_errors(line, normalized);
            let actual: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(
                actual["error"],
                serde_json::json!({"code":-33002,"message":"ACP request failed",
                    "data":{"kind":"turn_failed_after_dispatch","retryable":false}}),
                "{cause}",
            );
        }
    }

    #[test]
    fn legacy_batch_preserves_a_valid_provider_cause_and_drops_duplicates() {
        let base = r#""kind":"turn_failed_after_dispatch","retryable":false"#;
        let cause = r#""cause":{"kind":"provider_network","retryable":false}"#;
        let reply = |id, data| {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-33002,"message":"private peer text","data":{{{data}}}}}}}"#
            )
        };
        let sibling = r#"{"jsonrpc":"2.0", "id":4, "result":null}"#;
        let line = format!(
            "[{},{},{},{sibling}]",
            reply(1, format!("{base},{cause}")),
            reply(2, format!("{base},{cause},{cause}")),
            reply(3, format!("{base},\"retryable\":false,{cause}")),
        );
        let normalized = serde_json::from_str(&line).unwrap();
        let output = sanitize_public_errors(line, Some(normalized));
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            actual[0]["error"]["data"]["cause"],
            serde_json::json!({"kind":"provider_network","retryable":false})
        );
        for entry in &actual.as_array().unwrap()[1..3] {
            assert_eq!(
                entry["error"]["data"],
                serde_json::json!({"kind":"turn_failed_after_dispatch","retryable":false})
            );
        }
        assert!(!output.contains("private peer text"));
        assert!(output.contains(sibling));
    }

    #[test]
    fn provider_failure_drops_extra_envelope_fields_without_accepting_hybrids() {
        let data = r#"{"kind":"turn_failed_after_dispatch","retryable":false,"cause":{"kind":"provider_auth","retryable":false}}"#;
        for hybrid in ["", r#", "method": "private method""#, r#", "result": "private result""#] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-33002,"message":"private peer text","data":{data}}},"detail":"private envelope"{hybrid}}}"#
            );
            let normalized = serde_json::from_str(&line).unwrap();
            let output = sanitize_public_errors(line, Some(normalized));
            let actual: Value = serde_json::from_str(&output).unwrap();
            assert!(!output.contains("private"));
            assert_eq!(actual["error"]["data"], serde_json::from_str::<Value>(data).unwrap());
            if !hybrid.is_empty() {
                assert!(matches!(
                    agent_client_protocol::TransportFrame::parse_json(&output),
                    agent_client_protocol::TransportFrame::Malformed { .. }
                ));
            }
        }
    }

    #[test]
    fn model_refusal_drops_extra_error_members_before_sdk_trace() {
        let line = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32004,"message":"peer refusal","detail":"SECRET2",
            "data":{"kind":"model_selection_required","configId":"model","availableModels":2,"retryable":false}}}"#;
        let original: Value = serde_json::from_str(line).unwrap();
        let output = sanitize_public_errors(line.into(), Some(original.clone()));
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            actual["error"],
            serde_json::json!({"code":-32004,"message":ACP_REQUEST_FAILED,"data":original["error"]["data"]})
        );
        assert!(!output.contains("SECRET2"));
    }

    #[test]
    fn legacy_batch_preserves_valid_data_and_strips_only_invalid_refusal_data() {
        let line = r#"[
            {"jsonrpc":"2.0","id":1,"error":{"code":-32004,"message":"refusal",
                "data":{"kind":"model_selection_required","configId":"model","availableModels":0,"retryable":false}}},
            {"jsonrpc":"2.0","id":2,"error":{"code":-32004,"message":"refusal",
                "data":{"kind":"model_selection_required","configId":"model","availableModels":0,"availableModels":2,"retryable":false}}},
            {"jsonrpc":"2.0","id":3,"error":{"code":-32004,"message":"other refusal",
                "data":{"kind":"profile_unavailable","profile":"sample"}}},
            {"jsonrpc":"2.0","id":4,"result":null}
        ]"#;
        let original: Value = serde_json::from_str(line).unwrap();
        let output = sanitize_public_errors(line.into(), Some(original.clone()));
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual[0]["id"], 1);
        assert_eq!(actual[0]["error"]["code"], -32004);
        assert_eq!(actual[0]["error"]["message"], ACP_REQUEST_FAILED);
        assert_eq!(actual[0]["error"]["data"], original[0]["error"]["data"]);
        assert_eq!(actual[1]["id"], 2);
        assert_eq!(actual[1]["error"]["code"], -32004);
        assert_eq!(actual[1]["error"]["message"], ACP_REQUEST_FAILED);
        assert!(actual[1]["error"].get("data").is_none());
        assert_eq!(actual[2], original[2]);
        assert_eq!(actual[3], original[3]);
    }

    #[test]
    fn refusal_survives_a_data_value_that_the_sdk_cannot_normalize() {
        let line = r#"{"jsonrpc":"2.0","id":"prompt","error":{"code":-32004,"message":"refusal",
            "data":{"kind":"model_selection_required","configId":"model",
                "availableModels":{"$serde_json::private::RawValue":"invalid numeric content"},"retryable":false}}}"#;
        assert!(serde_json::from_str::<Value>(line).is_err());
        let output = sanitize_public_errors(line.into(), None);
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual["id"], "prompt");
        assert_eq!(actual["error"]["code"], -32004);
        assert_eq!(actual["error"]["message"], ACP_REQUEST_FAILED);
        assert!(actual["error"].get("data").is_none());

        let sibling = r#"{"jsonrpc":"2.0", "id":2, "result":null}"#;
        let batch = format!("[{line},{sibling}]");
        let output = sanitize_public_errors(batch, None);
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual[0]["error"]["code"], -32004);
        assert!(actual[0]["error"].get("data").is_none());
        assert_eq!(actual[1]["id"], 2);
        assert!(output.contains(sibling));
    }

    #[test]
    fn recovery_does_not_make_malformed_envelopes_into_error_responses() {
        let data =
            r#"{"kind":"model_selection_required","availableModels":{"$serde_json::private::RawValue":"invalid"}}"#;
        for frame in [
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":null,"error":{{"code":-32004,"message":"raw","data":{data}}}}}"#
            ),
            format!(
                r#"{{"jsonrpc":"2.0","id":1,"result":null,"error":{{"code":-32004,"message":"raw","data":{data}}}}}"#
            ),
            format!(r#"{{"jsonrpc":"2.0","id":1,"error":[-32004,"raw",{data}]}}"#),
            format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-32004,"message":null,"data":{data}}}}}"#),
        ] {
            assert!(serde_json::from_str::<Value>(&frame).is_err());
            assert_eq!(sanitize_public_errors(frame.clone(), None), frame);
        }
    }

    #[test]
    fn sanitization_preserves_a_missing_or_non_string_error_message() {
        let data = r#"{"kind":"model_selection_required","configId":"model","availableModels":2,"retryable":false}"#;
        for message in ["", r#""message":null,"#] {
            let frame = format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{{message}"code":-32004,"data":{data}}}}}"#);
            let normalized: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(sanitize_public_errors(frame.clone(), Some(normalized)), frame);
        }
    }
}
