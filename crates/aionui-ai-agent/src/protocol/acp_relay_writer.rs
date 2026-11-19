//! Ordered ACP writes with admission at the first byte of an extension frame.
use std::io;
use std::pin::Pin;
use std::task::Poll;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use super::acp_relay::{OutgoingFrame, RelayConnection, RelayError, check_stdio_line};

pub(super) async fn write_line(
    writer: &mut (impl AsyncWrite + Unpin),
    relay: &RelayConnection,
    local_replies: &UnboundedSender<String>,
    line: String,
) -> io::Result<()> {
    let frame = match relay.outgoing(line)? {
        OutgoingFrame::Drop => return Ok(()),
        OutgoingFrame::Standard(line) => {
            let line = if relay.candidate {
                check_stdio_line(line).map_err(io::Error::other)?
            } else {
                line
            };
            writer.write_all(line.as_bytes()).await?;
            return writer.write_all(b"\n").await;
        }
        OutgoingFrame::Relay(frame) => frame,
    };
    let mut line = check_stdio_line(frame.line).map_err(io::Error::other)?;
    line.push('\n');
    let bytes = line.as_bytes();
    let admission = frame.admission;
    let browser = admission.browser.as_ref();
    let mut checked = None;
    let first = std::future::poll_fn(|cx| {
        // After a WouldBlock poll the next pipe wake must obtain a fresh
        // authoritative account, ownership and session-generation snapshot.
        let check = checked.get_or_insert_with(|| Box::pin(admission.recheck()));
        match check.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(reason)) => return Poll::Ready(Err(reason)),
            Poll::Ready(Ok(())) => checked = None,
        }
        if tokio::time::Instant::now() >= admission.deadline {
            return Poll::Ready(Err(RelayError::DeadlineUnknown));
        }
        let mut result = None;
        admission.commit(&mut || {
            result = relay.first_write(&frame.id, frame.generation, || {
                Pin::new(&mut *writer).poll_write(cx, bytes)
            });
        });
        match result {
            Some(poll) => poll.map(Ok),
            None if browser.is_some_and(|guard| !guard.valid()) => Poll::Ready(Err(RelayError::AuthorizationRevoked)),
            None => Poll::Ready(Err(RelayError::StaleBinding)),
        }
    });
    let result = tokio::time::timeout_at(admission.deadline, first)
        .await
        .unwrap_or(Err(RelayError::DeadlineUnknown));
    let first_bytes = match result {
        Ok(result) => result?,
        Err(reason) => {
            if let Some(ack) = relay.reject_unsent(&frame.id, reason) {
                let _ = local_replies.send(ack);
            }
            return Ok(());
        }
    };
    if first_bytes == 0 {
        return Err(io::ErrorKind::WriteZero.into());
    }
    // Once any bytes were accepted, finish the line to preserve framing.
    // The service keeps the existing outcome-unknown semantics on revocation.
    writer.write_all(&bytes[first_bytes..]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use agent_client_protocol::schema::v1::{InitializeResponse, RequestId, SessionId};
    use aionui_api_types::AcpExtensionRequest;
    use aionui_common::WsSessionGuard;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, DuplexStream};

    struct ParkedPipe {
        pipe: DuplexStream,
        parked: Arc<tokio::sync::Notify>,
    }

    impl AsyncWrite for ParkedPipe {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let result = Pin::new(&mut self.pipe).poll_write(cx, bytes);
            if result.is_pending() {
                self.parked.notify_one();
            }
            result
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn acp_relay_rechecks_logout_generation_and_binding_after_pipe_backpressure() {
        let method = "_aelix/session/probe";
        for scenario in ["logout", "generation", "rebind", "positive", "revoke_after_first_byte"] {
            let relay = Arc::new(RelayConnection::new(true));
            let response: InitializeResponse = serde_json::from_value(json!({
                "protocolVersion":1,
                "agentInfo":{"name":"aelix-acp","version":"fixture"},
                "agentCapabilities":{"_meta":{"aelix/sessionRelay":{"version":1,"requests":[method],"notifications":[]}}}
            })).unwrap();
            relay.negotiate(&response);
            let mut meta = None;
            let attempt = relay.prepare_open(&mut meta);
            relay.opened(SessionId::new("fixture"), attempt, meta.as_ref());

            let token_valid = Arc::new(AtomicBool::new(true));
            let generation_valid = Arc::new(AtomicBool::new(true));
            let token = token_valid.clone();
            let generation = generation_valid.clone();
            let browser = WsSessionGuard::new(
                move || token.load(Ordering::Acquire),
                move || std::future::ready(generation.load(Ordering::Acquire)),
            );
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let admission =
                super::super::acp_relay::RelayWriteAdmission::new(Some(browser.clone()), deadline, move |write| {
                    browser.while_current(write)
                });
            let call = relay
                .dispatch(
                    AcpExtensionRequest {
                        binding_id: relay.discovery().unwrap().binding_id,
                        method: method.into(),
                        payload: json!({"marker":"private"}).as_object().unwrap().clone(),
                    },
                    admission,
                    |_| RequestId::Number(77),
                )
                .unwrap();
            let request = json!({"jsonrpc":"2.0","id":77,"method":method,"params":{}}).to_string();
            let (mut writer, mut reader) = tokio::io::duplex(64);
            writer.write_all(&[b'x'; 64]).await.unwrap();
            let parked = Arc::new(tokio::sync::Notify::new());
            let mut writer = ParkedPipe {
                pipe: writer,
                parked: parked.clone(),
            };
            let (local, mut acks) = tokio::sync::mpsc::unbounded_channel();
            let writing = tokio::spawn({
                let relay = relay.clone();
                async move { write_line(&mut writer, &relay, &local, request).await.unwrap() }
            });
            parked.notified().await;
            assert!(!writing.is_finished());
            let expected = match scenario {
                "logout" => {
                    token_valid.store(false, Ordering::Release);
                    Some(RelayError::AuthorizationRevoked)
                }
                "generation" => {
                    generation_valid.store(false, Ordering::Release);
                    Some(RelayError::AuthorizationRevoked)
                }
                "rebind" => {
                    let attempt = relay.prepare_open(&mut meta);
                    relay.opened(SessionId::new("replacement"), attempt, meta.as_ref());
                    Some(RelayError::StaleBinding)
                }
                _ => None,
            };
            let mut queued = vec![0; 64];
            reader.read_exact(&mut queued).await.unwrap();
            assert_eq!(queued, vec![b'x'; 64]);
            let mut sent = String::new();
            if scenario == "revoke_after_first_byte" {
                let mut first = [0];
                reader.read_exact(&mut first).await.unwrap();
                assert!(call.started.load(Ordering::Acquire));
                relay.invalidate();
                sent.push(char::from(first[0]));
            }
            reader.read_to_string(&mut sent).await.unwrap();
            writing.await.unwrap();
            if let Some(expected) = expected {
                assert!(sent.is_empty(), "{scenario} wrote an extension byte");
                assert_eq!(call.reply.await.unwrap(), Err(expected));
                let ack: serde_json::Value = serde_json::from_str(&acks.recv().await.unwrap()).unwrap();
                assert_eq!(ack, json!({"jsonrpc":"2.0","id":77,"result":null}));
            } else {
                let frame: serde_json::Value = serde_json::from_str(&sent).unwrap();
                assert_eq!(frame["params"]["payload"], json!({"marker":"private"}));
                assert!(acks.recv().await.is_none());
            }
        }
    }
}
