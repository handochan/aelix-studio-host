use std::{future::Future, pin::Pin, sync::Arc};

type RevocationFence = dyn Fn(&mut dyn FnMut()) + Send + Sync;

type Revalidation = dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync;

/// Browser session checks applied to a WebSocket after its initial admission.
#[derive(Clone)]
pub struct WsSessionGuard {
    current: Arc<dyn Fn() -> bool + Send + Sync>,
    revalidate: Arc<Revalidation>,
    revocation_fence: Option<Arc<RevocationFence>>,
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
            revocation_fence: None,
        }
    }

    pub fn with_revocation_fence(mut self, fence: impl Fn(&mut dyn FnMut()) + Send + Sync + 'static) -> Self {
        self.revocation_fence = Some(Arc::new(fence));
        self
    }

    /// Run a nonblocking write under the token issuer's revocation fence.
    pub fn while_current(&self, write: &mut dyn FnMut()) {
        let mut checked = || {
            if self.valid() {
                write();
            }
        };
        if let Some(fence) = &self.revocation_fence {
            fence(&mut checked);
        } else {
            checked();
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
