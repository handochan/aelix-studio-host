use std::{future::Future, pin::Pin, sync::Arc};

type Revalidation = dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync;

/// Browser session checks applied to a WebSocket after its initial admission.
#[derive(Clone)]
pub struct WsSessionGuard {
    current: Arc<dyn Fn() -> bool + Send + Sync>,
    revalidate: Arc<Revalidation>,
}

impl WsSessionGuard {
    /// Supply a frame-level check and a periodic check for server-side state.
    pub fn new<F>(
        current: impl Fn() -> bool + Send + Sync + 'static,
        revalidate: impl Fn() -> F + Send + Sync + 'static,
    ) -> Self
    where
        F: Future<Output = bool> + Send + 'static,
    {
        Self {
            current: Arc::new(current),
            revalidate: Arc::new(move || Box::pin(revalidate())),
        }
    }

    /// Check expiration and in-memory revocation before forwarding a frame.
    pub fn valid(&self) -> bool {
        (self.current)()
    }

    /// Also check the authoritative account and session generation.
    pub async fn recheck(&self) -> bool {
        self.valid() && (self.revalidate)().await
    }
}
