//! Model-selection refusal validation at the ACP stdio boundary.

use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use super::error::MODEL_SELECTION_REQUIRED;

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
        && reply["error"]["data"]["kind"].as_str() == Some(MODEL_SELECTION_REQUIRED)
}

fn drop_invalid_refusal_data(reply: &mut Value, raw: Option<&str>) -> bool {
    if !appears_to_be_model_selection_refusal(reply) || raw.is_some_and(valid_model_selection_refusal) {
        return false;
    }
    reply["error"].as_object_mut().unwrap().remove("data");
    true
}

/// The SDK first reads lines as `Value`, which folds duplicate keys and can
/// coerce special objects into scalars. Use that view to identify anything
/// the SDK would see as this refusal. Only the original JSON can validate it.
pub(super) fn drop_invalid_model_refusals(line: String, normalized: Option<Value>) -> String {
    let Some(mut normalized) = normalized else {
        // A syntactically valid data object can also fail Value decoding. Keep
        // a recognizable refusal's RPC code and correlation in that case.
        #[derive(Deserialize)]
        struct Tag {
            kind: String,
        }
        let stripped = (|| {
            let mut reply = wire_refusal(&line)?;
            let tag: Tag = serde_json::from_str(reply.error.data?.get()).ok()?;
            if tag.kind != MODEL_SELECTION_REQUIRED {
                return None;
            }
            reply.error.data = None;
            serde_json::to_string(&reply).ok()
        })();
        return stripped.unwrap_or(line);
    };
    let changed = if let Some(entries) = normalized.as_array_mut() {
        // Legacy connections still use the SDK's batch support.
        if !entries.iter().any(appears_to_be_model_selection_refusal) {
            return line;
        }
        let raw: Vec<&RawValue> = serde_json::from_str(&line).unwrap_or_default();
        let mut changed = false;
        for (index, reply) in entries.iter_mut().enumerate() {
            changed |= drop_invalid_refusal_data(reply, raw.get(index).map(|value| value.get()));
        }
        changed
    } else {
        drop_invalid_refusal_data(&mut normalized, Some(&line))
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
        let output = drop_invalid_model_refusals(line.into(), Some(original.clone()));
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual[0], original[0]);
        assert_eq!(actual[1]["id"], 2);
        assert_eq!(actual[1]["error"]["code"], -32004);
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
        let output = drop_invalid_model_refusals(line.into(), None);
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual["id"], "prompt");
        assert_eq!(actual["error"]["code"], -32004);
        assert!(actual["error"].get("data").is_none());
    }
}
