//! Serialize complete pipe frames, with priority for standard ACP traffic
//! while a relay frame has yet to write its first byte.
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use futures_util::Sink;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc::{self, UnboundedSender};

use super::acp_relay::{OutgoingFrame, RelayConnection, RelayError, RelayFrame, check_stdio_line};

const PIPE_RETRY: Duration = Duration::from_millis(5);

pub(super) fn sink(
    writer: impl AsyncWrite + Unpin + Send + 'static,
    relay: Arc<RelayConnection>,
    local: UnboundedSender<io::Result<String>>,
    alive: Arc<AtomicBool>,
) -> (impl Sink<String, Error = io::Error> + Send, tokio::task::JoinHandle<()>) {
    let (standard_tx, standard_rx) = mpsc::channel(8);
    // The application reserves a pending SDK slot before this queue.
    let (relay_tx, relay_rx) = mpsc::unbounded_channel();
    let wire = relay.clone();
    let driver = tokio::spawn(async move {
        if let Err(error) = run(writer, &wire, &local, standard_rx, relay_rx).await {
            alive.store(false, Ordering::Release);
            // Surface a broken stdin even if the child keeps stdout open.
            let _ = local.send(Err(error));
        }
    });
    let outgoing = futures_util::sink::unfold((standard_tx, relay_tx), move |(standard, extensions), line| {
        let relay = relay.clone();
        async move {
            match relay.outgoing(line)? {
                OutgoingFrame::Standard(line) => standard.send(line).await.map_err(|_| io::ErrorKind::BrokenPipe)?,
                OutgoingFrame::Relay(frame) => extensions.send(frame).map_err(|_| io::ErrorKind::BrokenPipe)?,
                OutgoingFrame::Drop => {}
            }
            Ok((standard, extensions))
        }
    });
    (outgoing, driver)
}

async fn run(
    mut writer: impl AsyncWrite + Unpin,
    relay: &RelayConnection,
    local: &UnboundedSender<io::Result<String>>,
    mut standard: mpsc::Receiver<String>,
    mut extensions: mpsc::UnboundedReceiver<RelayFrame>,
) -> io::Result<()> {
    loop {
        tokio::select! {
            biased;
            line = standard.recv() => match line {
                Some(line) => write_standard(&mut writer, line).await?,
                None => return Ok(()),
            },
            Some(frame) = extensions.recv() => {
                write_relay(&mut writer, relay, local, &mut standard, frame).await?;
            }
        }
    }
}

async fn write_standard(writer: &mut (impl AsyncWrite + Unpin), line: String) -> io::Result<()> {
    let mut line = check_stdio_line(line).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

async fn write_relay(
    writer: &mut (impl AsyncWrite + Unpin),
    relay: &RelayConnection,
    local: &UnboundedSender<io::Result<String>>,
    standard: &mut mpsc::Receiver<String>,
    frame: RelayFrame,
) -> io::Result<()> {
    let mut line = check_stdio_line(frame.line).map_err(io::Error::other)?;
    line.push('\n');
    let bytes = line.as_bytes();
    let admission = frame.admission;
    let deadline = admission.deadline;
    loop {
        let lock = tokio::select! {
            biased;
            line = standard.recv() => {
                let Some(line) = line else { return Ok(()) };
                write_standard(writer, line).await?;
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => Err(RelayError::DeadlineUnknown),
            result = admission.acquire() => result,
        };
        let lock = match lock {
            Ok(lock) => lock,
            Err(reason) => return reject(relay, local, &frame.id, reason),
        };

        // Don't retain the database fence across a standard write or pipe
        // backpressure. A preempted admission must acquire a fresh fence.
        if let Ok(line) = standard.try_recv() {
            drop(lock);
            write_standard(writer, line).await?;
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            drop(lock);
            return reject(relay, local, &frame.id, RelayError::DeadlineUnknown);
        }

        let attempt = std::future::poll_fn(|cx| {
            let mut result = None;
            admission.commit(&mut || {
                result = relay.first_write(&frame.id, frame.generation, || {
                    Pin::new(&mut *writer).poll_write(cx, bytes)
                });
            });
            Poll::Ready(result)
        })
        .await;
        drop(lock);
        match attempt {
            Some(Poll::Ready(result)) => {
                let written = result?;
                if written == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                // An accepted first byte makes the frame non-preemptible.
                writer.write_all(&bytes[written..]).await?;
                return writer.flush().await;
            }
            None => {
                let reason = if admission.browser.as_ref().is_some_and(|guard| !guard.valid()) {
                    RelayError::AuthorizationRevoked
                } else {
                    RelayError::StaleBinding
                };
                return reject(relay, local, &frame.id, reason);
            }
            Some(Poll::Pending) => {
                tokio::select! {
                    biased;
                    line = standard.recv() => {
                        let Some(line) = line else { return Ok(()) };
                        write_standard(writer, line).await?;
                    }
                    _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + PIPE_RETRY)) => {}
                }
            }
        }
    }
}

fn reject(
    relay: &RelayConnection,
    local: &UnboundedSender<io::Result<String>>,
    id: &agent_client_protocol::schema::v1::RequestId,
    reason: RelayError,
) -> io::Result<()> {
    if let Some(ack) = relay.reject_unsent(id, reason) {
        let _ = local.send(Ok(ack));
    }
    Ok(())
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

    struct TestLock;
    impl aionui_db::AcpRelayWriteLock for TestLock {}

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

    fn opened_relay(method: &str) -> Arc<RelayConnection> {
        let relay = Arc::new(RelayConnection::new(true));
        let response: InitializeResponse = serde_json::from_value(json!({
            "protocolVersion":1,
            "agentInfo":{"name":"aelix-acp","version":"fixture"},
            "agentCapabilities":{"_meta":{"aelix/sessionRelay":{"version":1,"requests":[method],"notifications":[]}}}
        }))
        .unwrap();
        relay.negotiate(&response);
        let mut meta = None;
        let attempt = relay.prepare_open(&mut meta);
        relay.opened(SessionId::new("fixture"), attempt, meta.as_ref());
        relay
    }

    #[tokio::test]
    async fn acp_relay_database_wait_does_not_delay_cancel_or_permission_response() {
        use futures_util::SinkExt as _;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let db = aionui_db::init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let held = pool.acquire().await.unwrap();
        let parked = Arc::new(tokio::sync::Notify::new());
        let revoked = Arc::new(AtomicBool::new(false));
        let method = "_aelix/session/probe";
        let relay = opened_relay(method);
        let call = relay
            .dispatch(
                AcpExtensionRequest {
                    binding_id: relay.discovery().unwrap().binding_id,
                    method: method.into(),
                    payload: json!({"marker":"private"}).as_object().unwrap().clone(),
                },
                super::super::acp_relay::RelayWriteAdmission::new(
                    None,
                    tokio::time::Instant::now() + Duration::from_secs(10),
                    |write| write(),
                )
                .with_database_fence({
                    let parked = parked.clone();
                    let revoked = revoked.clone();
                    move || {
                        let pool = pool.clone();
                        let parked = parked.clone();
                        let revoked = revoked.clone();
                        async move {
                            parked.notify_one();
                            let _lock = pool
                                .begin_with("BEGIN IMMEDIATE")
                                .await
                                .map_err(aionui_db::DbError::Query)?;
                            if revoked.load(Ordering::Acquire) {
                                Ok(aionui_db::AcpRelayWriteFence::SessionRevoked)
                            } else {
                                Ok(aionui_db::AcpRelayWriteFence::Granted(Box::new(TestLock)))
                            }
                            // The actual transaction is released when this
                            // synthetic admission returns.
                        }
                    }
                }),
                |_| RequestId::Number(77),
            )
            .unwrap();
        let (writer, reader) = tokio::io::duplex(1024);
        let (local, mut completion) = mpsc::unbounded_channel();
        let (sink, driver) = sink(writer, relay, local, Arc::new(AtomicBool::new(true)));
        let mut sink = Box::pin(sink);
        sink.send(json!({"jsonrpc":"2.0","id":77,"method":method,"params":{}}).to_string())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), parked.notified())
            .await
            .unwrap();
        let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"fixture"}});
        let permission = json!({"jsonrpc":"2.0","id":"permission-1",
            "result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
        for line in [&cancel, &permission] {
            sink.send(line.to_string()).await.unwrap();
        }
        let mut lines = BufReader::new(reader).lines();
        for expected in [cancel, permission] {
            let actual = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&actual).unwrap(), expected);
        }
        assert!(!call.started.load(Ordering::Acquire));
        assert!(completion.try_recv().is_err());
        revoked.store(true, Ordering::Release);
        drop(held);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), call.reply)
                .await
                .unwrap()
                .unwrap(),
            Err(RelayError::AuthorizationRevoked)
        );
        assert!(completion.recv().await.unwrap().is_ok());
        drop(sink);
        driver.await.unwrap();
        assert!(lines.next_line().await.unwrap().is_none());
    }

    /// Models the pinned Windows blocking adaptor: poll_write acknowledges
    /// scheduling, while poll_flush is the first observable pipe error.
    struct DeferredWriteError {
        accepted: bool,
    }

    impl AsyncWrite for DeferredWriteError {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.accepted = true;
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
            assert!(self.accepted);
            Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
            self.poll_flush(cx)
        }
    }

    #[tokio::test]
    async fn acp_relay_windows_simulated_standard_write_fails_while_stdout_remains_open() {
        use futures_util::SinkExt as _;

        let (_stdout_sender, mut stdout) = tokio::io::duplex(64);
        let (local, mut completion) = mpsc::unbounded_channel();
        let alive = Arc::new(AtomicBool::new(true));
        let line = json!({"jsonrpc":"2.0","id":9,"method":"session/prompt","params":{}}).to_string();
        let (sink, driver) = sink(
            DeferredWriteError { accepted: false },
            opened_relay("_aelix/session/probe"),
            local,
            alive.clone(),
        );
        let mut sink = Box::pin(sink);
        sink.send(line.clone()).await.unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), completion.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!alive.load(Ordering::Acquire));
        driver.await.unwrap();

        // Legacy agents use FramedWrite; its same-frame flush is also
        // necessary even though the peer's stdout has not reached EOF.
        let mut legacy = tokio_util::codec::FramedWrite::new(
            DeferredWriteError { accepted: false },
            tokio_util::codec::LinesCodec::new(),
        );
        assert!(legacy.send(line).await.is_err());
        let mut byte = [0];
        assert!(
            tokio::time::timeout(Duration::from_millis(10), stdout.read_exact(&mut byte))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn acp_relay_rechecks_logout_generation_and_binding_after_pipe_backpressure() {
        let method = "_aelix/session/probe";
        for scenario in ["logout", "generation", "rebind", "positive", "revoke_after_first_byte"] {
            let relay = opened_relay(method);
            let mut meta = None;

            let token_valid = Arc::new(AtomicBool::new(true));
            let generation_valid = Arc::new(AtomicBool::new(true));
            let token = token_valid.clone();
            let generation = generation_valid.clone();
            let browser = WsSessionGuard::new(move || token.load(Ordering::Acquire), || std::future::ready(true));
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            let admission =
                super::super::acp_relay::RelayWriteAdmission::new(Some(browser.clone()), deadline, move |write| {
                    browser.while_current(write)
                })
                .with_database_fence(move || {
                    let result = if generation.load(Ordering::Acquire) {
                        aionui_db::AcpRelayWriteFence::Granted(Box::new(TestLock))
                    } else {
                        aionui_db::AcpRelayWriteFence::SessionRevoked
                    };
                    std::future::ready(Ok(result))
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
            let (_std_tx, mut standard) = mpsc::channel(8);
            let OutgoingFrame::Relay(frame) = relay.outgoing(request).unwrap() else {
                panic!("relay frame")
            };
            let writing = tokio::spawn({
                let relay = relay.clone();
                async move {
                    write_relay(&mut writer, &relay, &local, &mut standard, frame)
                        .await
                        .unwrap()
                }
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
                let ack: serde_json::Value = serde_json::from_str(&acks.recv().await.unwrap().unwrap()).unwrap();
                assert_eq!(ack, json!({"jsonrpc":"2.0","id":77,"result":null}));
            } else {
                let frame: serde_json::Value = serde_json::from_str(&sent).unwrap();
                assert_eq!(frame["params"]["payload"], json!({"marker":"private"}));
                assert!(acks.recv().await.is_none());
            }
        }
    }
}
