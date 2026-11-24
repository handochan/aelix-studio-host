use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::{MAX_SSE_RESPONSE_BYTES, ResponseReadError};

/// Pending RPCs on one persistent SSE connection. Each has its own byte budget.
pub struct SseResponses<T>(Arc<Mutex<Responses<T>>>);

struct Responses<T> {
    // Keep delivered IDs reserved until their companion POST finishes.
    pending: HashMap<u64, Option<Pending<T>>>,
    closed: Option<ResponseReadError>,
}

struct Pending<T> {
    remaining: usize,
    reply: oneshot::Sender<Result<T, ResponseReadError>>,
}

#[derive(Debug, thiserror::Error)]
pub enum SseRequestError {
    #[error(transparent)]
    Closed(#[from] ResponseReadError),
    #[error("MCP SSE request id is already outstanding")]
    DuplicateId,
}

impl<T> Default for SseResponses<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Responses {
            pending: HashMap::new(),
            closed: None,
        })))
    }
}

impl<T> SseResponses<T> {
    /// Register immediately before sending the POST so an early reply is delivered.
    pub fn start(&self, id: u64) -> Result<SseResponse<T>, SseRequestError> {
        use std::collections::hash_map::Entry;

        let mut state = self.0.lock().unwrap();
        if let Some(error) = state.closed {
            return Err(error.into());
        }
        let Entry::Vacant(entry) = state.pending.entry(id) else {
            return Err(SseRequestError::DuplicateId);
        };
        let (reply, receiver) = oneshot::channel();
        entry.insert(Some(Pending {
            remaining: MAX_SSE_RESPONSE_BYTES,
            reply,
        }));
        Ok(SseResponse {
            id,
            responses: Arc::clone(&self.0),
            receiver,
        })
    }

    /// Charge wire bytes before dispatch, including fragments and unmatched events.
    pub fn charge(&self, bytes: usize) -> Result<(), ResponseReadError> {
        for pending in self.0.lock().unwrap().pending.values_mut().flatten() {
            pending.remaining = pending
                .remaining
                .checked_sub(bytes)
                .ok_or(ResponseReadError::BodyTooLarge)?;
        }
        Ok(())
    }

    /// Only a solicited reply is retained. Its byte window ends at delivery.
    pub fn deliver(&self, id: u64, response: T) {
        if let Some(pending) = self.0.lock().unwrap().pending.get_mut(&id).and_then(Option::take) {
            let _ = pending.reply.send(Ok(response));
        }
    }

    /// Terminate all waiters and reject future requests on this connection.
    pub fn close(&self, error: ResponseReadError) {
        let mut state = self.0.lock().unwrap();
        if state.closed.is_some() {
            return;
        }
        state.closed = Some(error);
        for pending in state.pending.drain().filter_map(|(_, pending)| pending) {
            let _ = pending.reply.send(Err(error));
        }
    }
}

impl<T> Clone for SseResponses<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Dropping a canceled or failed POST releases its window immediately.
pub struct SseResponse<T> {
    id: u64,
    responses: Arc<Mutex<Responses<T>>>,
    receiver: oneshot::Receiver<Result<T, ResponseReadError>>,
}

impl<T> SseResponse<T> {
    pub async fn receive(&mut self) -> Result<T, ResponseReadError> {
        (&mut self.receiver).await.unwrap_or(Err(ResponseReadError::ReadFailed))
    }
}

impl<T> Drop for SseResponse<T> {
    fn drop(&mut self) {
        self.responses.lock().unwrap().pending.remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn overlapping_waits_have_independent_budgets() {
        let responses = SseResponses::default();
        let mut first = responses.start(1).unwrap();
        responses.charge(MAX_SSE_RESPONSE_BYTES - 10).unwrap();
        let mut second = responses.start(2).unwrap();
        responses.charge(10).unwrap();
        responses.deliver(1, "first");
        assert_eq!(first.receive().await.unwrap(), "first");

        responses.charge(MAX_SSE_RESPONSE_BYTES - 10).unwrap();
        let error = responses.charge(1).unwrap_err();
        assert_eq!(error, ResponseReadError::BodyTooLarge);
        responses.close(error);
        assert_eq!(second.receive().await.unwrap_err(), error);
    }

    #[tokio::test]
    async fn unmatched_frames_and_canceled_waits_do_not_reset_other_windows() {
        let responses = SseResponses::default();
        let mut current = responses.start(1).unwrap();
        let canceled = responses.start(2).unwrap();
        responses.deliver(42, "unsolicited");
        responses.charge(MAX_SSE_RESPONSE_BYTES).unwrap();
        drop(canceled);
        let mut later = responses.start(2).unwrap();
        let error = responses.charge(1).unwrap_err();
        assert_eq!(error, ResponseReadError::BodyTooLarge);
        responses.close(error);
        assert_eq!(current.receive().await.unwrap_err(), error);
        assert_eq!(later.receive().await.unwrap_err(), error);
    }

    #[test]
    fn idle_bytes_do_not_enter_the_next_budget() {
        let responses = SseResponses::<()>::default();
        responses.charge(MAX_SSE_RESPONSE_BYTES + 1).unwrap();
        {
            let _waiting = responses.start(1).unwrap();
            responses.charge(MAX_SSE_RESPONSE_BYTES).unwrap();
        }
        responses.charge(MAX_SSE_RESPONSE_BYTES + 1).unwrap();
        let _waiting = responses.start(2).unwrap();
        responses.charge(MAX_SSE_RESPONSE_BYTES).unwrap();
    }
}
