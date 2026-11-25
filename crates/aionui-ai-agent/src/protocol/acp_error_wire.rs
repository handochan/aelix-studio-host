//! Public application error validation at the ACP stdio boundary.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
};

use agent_client_protocol::schema::v1::RequestId;
use serde::{Deserialize, Deserializer, Serialize, de::IgnoredAny};
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

#[derive(Deserialize)]
struct ResponseEnvelope<'a> {
    jsonrpc: String,
    id: RequestId,
    #[serde(default, borrow)]
    error: Option<&'a RawValue>,
    #[serde(default, deserialize_with = "member_present")]
    method: bool,
    #[serde(default, deserialize_with = "member_present")]
    result: bool,
}

fn member_present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(deserializer).map(|_| true)
}

#[derive(Deserialize)]
struct ErrorHeader {
    code: i32,
    #[serde(rename = "message")]
    _message: String,
}

#[derive(Deserialize)]
struct ErrorData<'a> {
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct ErrorTag {
    kind: String,
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

fn wire_provider_cause(data: &RawValue) -> Option<Value> {
    let data = data.get();
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

fn sanitize_provider_reply(
    raw: &str,
    normalized: Option<&Value>,
    correlate: &mut dyn FnMut(&RequestId) -> bool,
) -> Option<String> {
    if !raw.trim_start().starts_with('{') {
        return None;
    }
    let envelope: ResponseEnvelope<'_> = serde_json::from_str(raw).ok()?;
    if envelope.jsonrpc != "2.0" || envelope.id == RequestId::Null {
        return None;
    }
    let Some(error) = envelope.error else {
        if !envelope.method && envelope.result && normalized.is_some_and(|value| value.get("error").is_none()) {
            correlate(&envelope.id);
        }
        return None;
    };
    if !error.get().trim_start().starts_with('{') {
        return None;
    }
    // Identify the error from the raw code independently of data. A duplicated
    // data member or tag is ambiguous, so it cannot opt out of redaction.
    let header: ErrorHeader = serde_json::from_str(error.get()).ok()?;
    let data = serde_json::from_str::<ErrorData<'_>>(error.get())
        .ok()
        .and_then(|error| error.data);
    let unrelated = data
        .filter(|data| data.get().trim_start().starts_with('{'))
        .and_then(|data| serde_json::from_str::<ErrorTag>(data.get()).ok())
        .is_some_and(|tag| !tag.kind.is_empty() && tag.kind != TURN_FAILED_AFTER_DISPATCH);
    let hybrid = envelope.method || envelope.result;
    if header.code != -33002 || unrelated {
        if !hybrid && normalized.is_some_and(|value| value["error"]["message"].is_string()) {
            correlate(&envelope.id);
        }
        return None;
    }
    if !hybrid && !correlate(&envelope.id) {
        return None;
    }
    Some(
        provider_reply(
            envelope.id,
            envelope.method,
            envelope.result,
            data.and_then(wire_provider_cause),
        )
        .to_string(),
    )
}

fn provider_reply(id: RequestId, method: bool, result: bool, cause: Option<Value>) -> Value {
    let mut reply = json!({"jsonrpc":"2.0", "id":id, "error":{
        "code": -33002, "message": ACP_REQUEST_FAILED, "data": turn_failure_data(cause),
    }});
    // Preserve the SDK's rejection of hybrids without retaining their payloads.
    if method {
        reply["method"] = Value::Null;
    }
    if result {
        reply["result"] = Value::Null;
    }
    reply
}

pub(super) fn normalized_provider_reply_id(reply: &Value) -> Option<RequestId> {
    if reply["jsonrpc"] != "2.0"
        || reply["error"]["code"].as_i64() != Some(-33002)
        || !reply["error"]["message"].is_string()
        || reply["error"]["data"]["kind"].as_str() != Some(TURN_FAILED_AFTER_DISPATCH)
    {
        return None;
    }
    let id = RequestId::deserialize(&reply["id"]).ok()?;
    (id != RequestId::Null).then_some(id)
}

fn sanitize_normalized_provider_reply(reply: &mut Value, correlate: &mut dyn FnMut(&RequestId) -> bool) -> bool {
    let Some(id) = normalized_provider_reply_id(reply) else {
        return false;
    };
    let method = reply.get("method").is_some();
    let result = reply.get("result").is_some();
    if !method && !result && !correlate(&id) {
        return false;
    }
    // Value can synthesize replies or collapse ambiguous keys. Keep only the
    // generic error when the recognized reply lacks a directly validated cause.
    *reply = provider_reply(id, method, result, None);
    true
}

fn sanitize_model_refusal(reply: &mut Value, raw: Option<&str>) -> bool {
    if !reply["error"]["message"].is_string()
        || reply["error"]["code"].as_i64() != Some(-32004)
        || reply["error"]["data"]["kind"].as_str() != Some(MODEL_SELECTION_REQUIRED)
    {
        return false;
    }
    let mut error = json!({"code": -32004, "message": ACP_REQUEST_FAILED});
    if raw.is_some_and(valid_model_selection_refusal) {
        error["data"] = reply["error"]["data"].take();
    }
    reply["error"] = error;
    true
}

fn recover_undecodable_model_refusal(line: &str) -> Option<String> {
    // Match the SDK's response boundary. Projecting a hybrid or a
    // sequence-shaped error into a response would falsely confirm rejection.
    let envelope: BTreeMap<String, &RawValue> = serde_json::from_str(line).ok()?;
    if envelope.contains_key("method")
        || envelope.contains_key("result")
        || !envelope.get("error")?.get().trim_start().starts_with('{')
    {
        return None;
    }
    let mut reply = wire_reply(line, -32004)?;
    // A response with a missing or non-string message is invalid even if the
    // error's code and kind can be read from the raw bytes.
    serde_json::from_str::<String>(reply.error.message.get()).ok()?;
    let data = reply.error.data?.get();
    if !data.trim_start().starts_with('{')
        || serde_json::from_str::<ErrorTag>(data).ok()?.kind != MODEL_SELECTION_REQUIRED
    {
        return None;
    }
    let message = serde_json::value::to_raw_value(ACP_REQUEST_FAILED).ok()?;
    reply.error.message = &message;
    reply.error.data = None;
    serde_json::to_string(&reply).ok()
}

/// Project provider failures from raw replies for the initialized Aelix peer.
/// Use the SDK's normalized view to identify model refusals, but validate their
/// data against the original JSON, which retains duplicate keys and types.
pub(super) fn sanitize_public_errors(
    line: String,
    normalized: Option<Value>,
    pending_aelix_reply: Option<&dyn Fn(&RequestId) -> bool>,
) -> (String, HashSet<RequestId>) {
    let check_batch = normalized.is_none() && line.trim_start().starts_with('[');
    let mut completed = HashSet::new();
    let rewritten = if let Some(pending) = pending_aelix_reply {
        project_public_errors(
            line,
            normalized,
            Some(&mut |id| pending(id) && completed.insert(id.clone())),
        )
    } else {
        project_public_errors(line, normalized, None)
    };
    // The SDK has to decode the whole batch before correlating any member.
    if check_batch && !completed.is_empty() && serde_json::from_str::<Value>(&rewritten).is_err() {
        completed.clear();
    }
    (rewritten, completed)
}

fn project_public_errors(
    line: String,
    normalized: Option<Value>,
    mut correlate_aelix_reply: Option<&mut dyn FnMut(&RequestId) -> bool>,
) -> String {
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
                let normalized = serde_json::from_str::<Value>(raw).ok();
                let rewritten = correlate_aelix_reply
                    .as_mut()
                    .and_then(|correlate| sanitize_provider_reply(raw, normalized.as_ref(), *correlate))
                    .or_else(|| match normalized {
                        Some(mut reply) => (correlate_aelix_reply
                            .as_mut()
                            .is_some_and(|correlate| sanitize_normalized_provider_reply(&mut reply, *correlate))
                            || sanitize_model_refusal(&mut reply, Some(raw)))
                        .then(|| reply.to_string()),
                        None => recover_undecodable_model_refusal(raw),
                    });
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
    if let Some(correlate) = correlate_aelix_reply.as_mut()
        && let Some(reply) = sanitize_provider_reply(&line, normalized.as_ref(), *correlate)
    {
        return reply;
    }
    let Some(mut normalized) = normalized else {
        // A syntactically valid data object can also fail Value decoding.
        return recover_undecodable_model_refusal(&line).unwrap_or(line);
    };
    let changed = if let Some(entries) = normalized.as_array_mut() {
        // The normalized view can expose a batch synthesized from a raw
        // object. No entry has an original JSON object to validate against.
        let mut changed = false;
        for reply in entries {
            changed |= correlate_aelix_reply
                .as_mut()
                .is_some_and(|correlate| sanitize_normalized_provider_reply(reply, *correlate))
                || sanitize_model_refusal(reply, None);
        }
        changed
    } else {
        correlate_aelix_reply.is_some_and(|correlate| sanitize_normalized_provider_reply(&mut normalized, correlate))
            || sanitize_model_refusal(&mut normalized, Some(&line))
    };
    if changed { normalized.to_string() } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_aelix_reply(line: String, normalized: Option<Value>) -> String {
        sanitize_public_errors(line, normalized, Some(&|_| true)).0
    }

    #[test]
    fn malformed_turn_failure_data_keeps_the_correlated_error_without_peer_text() {
        let cases = [
            r#"{"kind":false,"detail":"private peer data"}"#,
            r#"{"kind":"turn_failed_after_dispatch","kind":"unrelated","retryable":false,"cause":{"kind":"provider_auth","retryable":1e400}}"#,
            r#"{"kind":"unrelated","kind":"turn_failed_after_dispatch","cause":{"kind":"provider_auth","retryable":1e400}}"#,
            r#"{"kind":"unrelated"},"data":{"kind":"unrelated","number":1e400}"#,
            r#"["unrelated"]"#,
            "null",
        ];
        let errors: Vec<_> = cases
            .iter()
            .map(|data| {
                let line = format!(
                    r#"{{"jsonrpc":"2.0","id":1,"error":{{"code":-33002,"message":"private peer text","data":{data}}}}}"#
                );
                let normalized = serde_json::from_str(&line).ok();
                serde_json::from_str::<Value>(&project_aelix_reply(line, normalized)).ok()
            })
            .collect();
        let expected = json!({"jsonrpc":"2.0","id":1,"error":{"code":-33002,"message":ACP_REQUEST_FAILED,
            "data":{"kind":"turn_failed_after_dispatch","retryable":false}}});
        assert_eq!(errors, vec![Some(expected); cases.len()]);
    }

    #[test]
    fn provider_rewrite_uses_pending_ids_and_preserves_other_tagged_errors() {
        let other = r#"{"jsonrpc":"2.0", "id":1, "error":{"code":-33002,"message":"other failure","data":{"kind":"other_failure"}}}"#;
        let success = r#"{"jsonrpc":"2.0", "id":2, "result":null}"#;
        let reply = |id| {
            format!(
                r#"{{"jsonrpc":"2.0", "id":{id}, "error":{{"code":-33002,"message":"peer text","data":{{"kind":false}}}}}}"#
            )
        };
        let late = reply(2);
        let correlated = reply(3);
        let line = format!("[{other},{success},{late},{correlated}]");
        let normalized = serde_json::from_str(&line).ok();
        let pending = HashSet::from([RequestId::Number(1), RequestId::Number(2), RequestId::Number(3)]);
        let (output, completed) = sanitize_public_errors(line, normalized, Some(&|id| pending.contains(id)));
        assert_eq!(completed, pending);
        assert!(output.contains(other) && output.contains(success) && output.contains(&late));
        let rewritten: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(rewritten[3]["id"], 3);
        assert_eq!(
            rewritten[3]["error"],
            json!({"code":-33002,"message":ACP_REQUEST_FAILED,
            "data":{"kind":"turn_failed_after_dispatch","retryable":false}})
        );
    }

    #[test]
    fn provider_rewrite_does_not_claim_invalid_or_ambiguous_reply_headers() {
        for reply in [
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-33002,"data":{"kind":false}}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-33002,"message":null,"data":{"kind":false}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"id":1,"error":{"code":-33002,"message":"peer text","data":{"kind":false}}}"#,
        ] {
            let (rewritten, completed) = sanitize_public_errors(
                reply.into(),
                serde_json::from_str(reply).ok(),
                Some(&|_| panic!("invalid or ambiguous reply header consumed a request")),
            );
            assert_eq!(rewritten, reply);
            assert!(completed.is_empty());
        }
    }

    #[test]
    fn undecodable_legacy_batch_does_not_complete_requests() {
        let line = r#"[{"jsonrpc":"2.0","id":1,"result":null},{"private":1e400}]"#;
        let pending = |id: &RequestId| id == &RequestId::Number(1);
        let (rewritten, completed) = sanitize_public_errors(line.into(), None, Some(&pending));
        assert_eq!(rewritten, line);
        assert!(completed.is_empty());

        let reply = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-33002,"message":"peer text","data":{"kind":false}}}"#;
        let (rewritten, completed) =
            sanitize_public_errors(reply.into(), serde_json::from_str(reply).ok(), Some(&pending));
        assert_eq!(completed, HashSet::from([RequestId::Number(1)]));
        assert!(!rewritten.contains("peer text"));
    }

    #[test]
    fn provider_replies_known_only_after_normalization_lose_the_cause() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-33002,"message":"peer text","data":{"kind":"turn_failed_after_dispatch","retryable":false,"cause":{"kind":"provider_auth","retryable":false}}}}"#;
        let duplicate_code = reply.replace("\"code\":-33002", "\"code\":-1,\"code\":-33002");
        let wrapped = json!({"$serde_json::private::RawValue": reply}).to_string();
        let wrapped_batch = json!({"$serde_json::private::RawValue": format!("[{reply}]")}).to_string();
        let pending = |id: &RequestId| id == &RequestId::Number(1);
        for line in [duplicate_code, wrapped, wrapped_batch] {
            let normalized = serde_json::from_str(&line).unwrap();
            let (rewritten, completed) = sanitize_public_errors(line, Some(normalized), Some(&pending));
            assert_eq!(completed, HashSet::from([RequestId::Number(1)]));
            assert!(!rewritten.contains("peer text"));
            let value: Value = serde_json::from_str(&rewritten).unwrap();
            let error = if value.is_array() {
                &value[0]["error"]
            } else {
                &value["error"]
            };
            assert_eq!(
                error,
                &json!({"code":-33002,"message":ACP_REQUEST_FAILED,
                "data":{"kind":"turn_failed_after_dispatch","retryable":false}})
            );
        }
    }

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
            let output = project_aelix_reply(line, normalized);
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
        let output = project_aelix_reply(line, Some(normalized));
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
            let output = project_aelix_reply(line, Some(normalized));
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
        let output = project_aelix_reply(line.into(), Some(original.clone()));
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
        let output = project_aelix_reply(line.into(), Some(original.clone()));
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
        let output = project_aelix_reply(line.into(), None);
        let actual: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(actual["id"], "prompt");
        assert_eq!(actual["error"]["code"], -32004);
        assert_eq!(actual["error"]["message"], ACP_REQUEST_FAILED);
        assert!(actual["error"].get("data").is_none());

        let sibling = r#"{"jsonrpc":"2.0", "id":2, "result":null}"#;
        let batch = format!("[{line},{sibling}]");
        let output = project_aelix_reply(batch, None);
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
            assert_eq!(project_aelix_reply(frame.clone(), None), frame);
        }
    }

    #[test]
    fn sanitization_preserves_a_missing_or_non_string_error_message() {
        let data = r#"{"kind":"model_selection_required","configId":"model","availableModels":2,"retryable":false}"#;
        for message in ["", r#""message":null,"#] {
            let frame = format!(r#"{{"jsonrpc":"2.0","id":1,"error":{{{message}"code":-32004,"data":{data}}}}}"#);
            let normalized: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(project_aelix_reply(frame.clone(), Some(normalized)), frame);
        }
    }
}
