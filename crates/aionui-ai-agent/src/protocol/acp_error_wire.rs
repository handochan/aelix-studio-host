//! Model-selection refusal validation at the ACP stdio boundary.

use std::{borrow::Cow, collections::BTreeMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use super::error::{ACP_REQUEST_FAILED, MODEL_SELECTION_REQUIRED};

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

fn wire_refusal(raw: &str) -> Option<WireReply<'_>> {
    let reply: WireReply<'_> = serde_json::from_str(raw).ok()?;
    (reply.error.code == -32004).then_some(reply)
}

fn valid_model_selection_refusal(raw: &str) -> bool {
    wire_refusal(raw)
        .and_then(|reply| serde_json::from_str::<ModelSelectionRefusal>(reply.error.data?.get()).ok())
        .is_some_and(|data| data.kind == MODEL_SELECTION_REQUIRED && data.config_id == "model" && !data.retryable)
}

fn appears_to_be_model_selection_refusal(reply: &Value) -> bool {
    reply["error"]["code"].as_i64() == Some(-32004)
        && reply["error"]["message"].is_string()
        && reply["error"]["data"]["kind"].as_str() == Some(MODEL_SELECTION_REQUIRED)
}

fn sanitize_refusal(reply: &mut Value, raw: Option<&str>) -> bool {
    if !appears_to_be_model_selection_refusal(reply) {
        return false;
    }
    let error = reply["error"].as_object_mut().unwrap();
    if !raw.is_some_and(valid_model_selection_refusal) {
        error.remove("data");
    }
    error.insert("message".into(), Value::String(ACP_REQUEST_FAILED.into()));
    true
}

fn recover_undecodable_refusal(line: &str) -> Option<String> {
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
    let mut reply = wire_refusal(line)?;
    // A response with a missing or non-string message is invalid even if the
    // refusal's code and kind can be read from the raw bytes.
    serde_json::from_str::<String>(reply.error.message.get()).ok()?;
    let data = reply.error.data?.get();
    if !data.trim_start().starts_with('{') || serde_json::from_str::<Tag>(data).ok()?.kind != MODEL_SELECTION_REQUIRED {
        return None;
    }
    let message = serde_json::value::to_raw_value(ACP_REQUEST_FAILED).ok()?;
    reply.error.message = &message;
    reply.error.data = None;
    serde_json::to_string(&reply).ok()
}

/// The SDK first reads lines as `Value`, which folds duplicate keys and can
/// coerce special objects into scalars. Use that view to identify anything
/// the SDK would see as this refusal. Only the original JSON can validate it.
/// Replace peer text here so SDK and host diagnostics see the public detail.
pub(super) fn sanitize_model_refusals(line: String, normalized: Option<Value>) -> String {
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
                    Ok(mut reply) => sanitize_refusal(&mut reply, Some(raw)).then(|| reply.to_string()),
                    Err(_) => recover_undecodable_refusal(raw),
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
        return recover_undecodable_refusal(&line).unwrap_or(line);
    };
    let changed = if let Some(entries) = normalized.as_array_mut() {
        // The normalized view can expose a batch synthesized from a raw
        // object. No entry has an original JSON object to validate against.
        let mut changed = false;
        for reply in entries {
            changed |= sanitize_refusal(reply, None);
        }
        changed
    } else {
        sanitize_refusal(&mut normalized, Some(&line))
    };
    if changed { normalized.to_string() } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let output = sanitize_model_refusals(line.into(), Some(original.clone()));
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
        let output = sanitize_model_refusals(line.into(), None);
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual["id"], "prompt");
        assert_eq!(actual["error"]["code"], -32004);
        assert_eq!(actual["error"]["message"], ACP_REQUEST_FAILED);
        assert!(actual["error"].get("data").is_none());

        let sibling = r#"{"jsonrpc":"2.0", "id":2, "result":null}"#;
        let batch = format!("[{line},{sibling}]");
        let output = sanitize_model_refusals(batch, None);
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
            assert_eq!(sanitize_model_refusals(frame.clone(), None), frame);
        }
    }

    #[test]
    fn sanitization_preserves_a_missing_or_non_string_error_message() {
        let data = r#"{"kind":"model_selection_required","configId":"model","availableModels":2,"retryable":false}"#;
        for message in ["", r#""message":null,"#] {
            let frame = format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{{message}"code":-32004,"data":{data}}}}}"#);
            let normalized: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(sanitize_model_refusals(frame.clone(), Some(normalized)), frame);
        }
    }
}
