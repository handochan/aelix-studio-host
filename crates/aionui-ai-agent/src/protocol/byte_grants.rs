//! RAM-only byte source for an authenticated ACP relay binding.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use aionui_api_types::{ByteGrantDescriptor, ByteGrantPrepared, ChatFileRef};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::acp_relay::RelayWriteAdmission;

pub(super) const META_KEY: &str = "aelix/hostBytes";
pub(super) const METHOD: &str = "_aelix/host/bytes/read";
pub(super) const FRAME_LIMIT: usize = 192 * 1024;
pub(super) const MAX_ID_BYTES: usize = 1024;
pub const SNAPSHOT_LIMIT: usize = 6 * 1024 * 1024;
pub const REF_LIMIT: usize = 4;
const MAX_CHUNK: usize = 147_264;
const RAM_LIMIT: usize = 32 * 1024 * 1024;
const RECORD_LIMIT: usize = 256;
const PREPARE_TTL: Duration = Duration::from_secs(30);
const UNUSED_TTL: Duration = Duration::from_secs(60);
const READ_TTL: Duration = Duration::from_secs(30);
const FINISHED_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteGrantError {
    Conflict,
    Limit,
    File,
    Expired,
    Revoked,
}

#[derive(Default)]
pub struct ByteGrantPool(Mutex<Budget>);

#[derive(Default)]
struct Budget {
    preparing: HashMap<String, usize>,
    bytes: usize,
    records: usize,
}

struct Reservation {
    pool: Arc<ByteGrantPool>,
    owner: Option<String>,
    bytes: usize,
}

impl ByteGrantPool {
    fn reserve(self: &Arc<Self>, owner: &str) -> Result<Reservation, ByteGrantError> {
        let mut budget = self.0.lock().unwrap();
        if budget.records == RECORD_LIMIT
            || budget.bytes + SNAPSHOT_LIMIT > RAM_LIMIT
            || budget.preparing.get(owner).copied().unwrap_or(0) == 2
            || budget.preparing.values().sum::<usize>() == 4
        {
            return Err(ByteGrantError::Limit);
        }
        *budget.preparing.entry(owner.into()).or_default() += 1;
        budget.bytes += SNAPSHOT_LIMIT;
        budget.records += 1;
        Ok(Reservation {
            pool: self.clone(),
            owner: Some(owner.into()),
            bytes: SNAPSHOT_LIMIT,
        })
    }
}

impl Reservation {
    fn settle(&mut self, bytes: usize) {
        let mut budget = self.pool.0.lock().unwrap();
        if let Some(owner) = self.owner.take() {
            let count = budget.preparing.get_mut(&owner).unwrap();
            *count -= 1;
            if *count == 0 {
                budget.preparing.remove(&owner);
            }
        }
        budget.bytes -= self.bytes - bytes;
        self.bytes = bytes;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.settle(0);
        self.pool.0.lock().unwrap().records -= 1;
    }
}

#[derive(Default)]
pub struct ByteGrantBinding(Mutex<Binding>);

#[derive(Default)]
struct Binding {
    closed: bool,
    batches: HashMap<String, Batch>,
}

type PreparationResult = Result<Prepared, ByteGrantError>;

struct Batch {
    fingerprint: [u8; 32],
    session_generation: Option<i64>,
    admission: Arc<RelayWriteAdmission>,
    reservation: Arc<Mutex<Reservation>>,
    result: watch::Sender<Option<PreparationResult>>,
    prepare_deadline: Instant,
    _cancel_on_drop: DropGuard,
    grants: Vec<Grant>,
}

#[derive(Clone)]
struct Prepared {
    grants: Vec<ByteGrantDescriptor>,
    expires_at_ms: i64,
    expires_at: Instant,
}

struct Grant {
    descriptor: ByteGrantDescriptor,
    bytes: Option<Vec<u8>>,
    deadline: Instant,
    reader: Option<Reader>,
}

struct Reader {
    id: String,
    last: std::ops::Range<usize>,
}

pub struct BytePreparation {
    result: watch::Receiver<Option<PreparationResult>>,
    deadline: Instant,
}

impl BytePreparation {
    pub async fn result(mut self) -> Result<ByteGrantPrepared, ByteGrantError> {
        let prepared = tokio::time::timeout_at(self.deadline, self.result.wait_for(Option::is_some))
            .await
            .map_err(|_| ByteGrantError::Expired)?
            .map_err(|_| ByteGrantError::Revoked)?
            .clone()
            .unwrap()?;
        Ok(ByteGrantPrepared {
            grants: prepared.grants,
            expires_at_ms: prepared.expires_at_ms,
            expires_in_ms: prepared
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_millis() as u64,
        })
    }
}

impl ByteGrantBinding {
    pub(super) fn close(&self) {
        let mut binding = self.0.lock().unwrap();
        binding.closed = true;
        binding.batches.clear();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare<F>(
        self: &Arc<Self>,
        pool: &Arc<ByteGrantPool>,
        owner: &str,
        prepare_id: String,
        refs: &[ChatFileRef],
        session_generation: Option<i64>,
        admission: RelayWriteAdmission,
        snapshot: impl FnOnce(CancellationToken) -> F,
    ) -> Result<BytePreparation, ByteGrantError>
    where
        F: Future<Output = Result<Vec<Vec<u8>>, ByteGrantError>> + Send + 'static,
    {
        let fingerprint = Sha256::digest(serde_json::to_vec(refs).map_err(|_| ByteGrantError::File)?).into();
        let mut binding = self.0.lock().unwrap();
        if binding.closed || admission.browser.as_ref().is_some_and(|browser| !browser.valid()) {
            return Err(ByteGrantError::Revoked);
        }
        binding.prune(Instant::now());
        if let Some(batch) = binding.batches.get(&prepare_id) {
            if batch.fingerprint != fingerprint || batch.session_generation != session_generation {
                return Err(ByteGrantError::Conflict);
            }
            return Ok(BytePreparation {
                result: batch.result.subscribe(),
                deadline: batch.prepare_deadline,
            });
        }
        if binding.batches.values().any(|batch| batch.result.borrow().is_none()) {
            return Err(ByteGrantError::Limit);
        }
        let reservation = Arc::new(Mutex::new(pool.reserve(owner)?));
        let (result, receiver) = watch::channel(None);
        let lifetime = receiver.clone();
        let deadline = Instant::now() + PREPARE_TTL;
        let admission = Arc::new(admission);
        let cancel = CancellationToken::new();
        binding.batches.insert(
            prepare_id.clone(),
            Batch {
                fingerprint,
                session_generation,
                admission: admission.clone(),
                reservation: reservation.clone(),
                result,
                prepare_deadline: deadline,
                _cancel_on_drop: cancel.clone().drop_guard(),
                grants: Vec::new(),
            },
        );
        let binding = Arc::downgrade(self);
        let snapshot = snapshot(cancel.clone());
        tokio::spawn(async move {
            let record = Arc::downgrade(&reservation);
            let cancelled = async {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => ByteGrantError::Revoked,
                    _ = tokio::time::sleep_until(deadline) => ByteGrantError::Expired,
                }
            };
            tokio::pin!(snapshot, cancelled);
            let copied = tokio::select! {
                biased;
                error = &mut cancelled => {
                    Self::fail_preparation(&binding, &prepare_id, &record, error);
                    cancel.cancel();
                    // Keep owner/Host slots and the RAM charge until the
                    // copy returns, including a syscall that cannot be interrupted.
                    drop(snapshot.await);
                    return;
                }
                copied = &mut snapshot => copied,
            };
            let result = match copied {
                Ok(bytes) => {
                    let mut grants = Some(make_grants(bytes));
                    tokio::select! {
                        biased;
                        error = &mut cancelled => Err(error),
                        lock = admission.acquire() => {
                            match lock {
                                Ok(_lock) => {
                                    let mut published = false;
                                    admission.commit(&mut || {
                                        if let Some(binding) = binding.upgrade() {
                                            published = binding.publish(&prepare_id, &reservation, grants.take().unwrap());
                                        }
                                    });
                                    if published { Ok(()) } else { Err(ByteGrantError::Revoked) }
                                }
                                Err(_) => Err(ByteGrantError::Revoked),
                            }
                        }
                    }
                }
                Err(error) => Err(error),
            };
            drop(reservation);
            if let Err(error) = result {
                Self::fail_preparation(&binding, &prepare_id, &record, error);
                return;
            }
            Self::expire(binding, prepare_id, record, lifetime).await;
        });
        Ok(BytePreparation {
            result: receiver,
            deadline,
        })
    }

    fn fail_preparation(binding: &Weak<Self>, key: &str, record: &Weak<Mutex<Reservation>>, error: ByteGrantError) {
        if let Some(binding) = binding.upgrade() {
            let mut inner = binding.0.lock().unwrap();
            if inner
                .batches
                .get(key)
                .is_some_and(|batch| record.as_ptr() == Arc::as_ptr(&batch.reservation))
            {
                let batch = inner.batches.remove(key).unwrap();
                batch.result.send_replace(Some(Err(error)));
            }
        }
    }

    fn publish(&self, key: &str, reservation: &Arc<Mutex<Reservation>>, grants: Vec<Grant>) -> bool {
        let mut binding = self.0.lock().unwrap();
        let Some(batch) = binding.batches.get_mut(key) else {
            return false;
        };
        if !Arc::ptr_eq(&batch.reservation, reservation) || Instant::now() >= batch.prepare_deadline {
            return false;
        }
        reservation
            .lock()
            .unwrap()
            .settle(grants.iter().map(|grant| grant.descriptor.byte_length).sum());
        let now = Instant::now();
        let prepared = Prepared {
            grants: grants.iter().map(|grant| grant.descriptor.clone()).collect(),
            expires_at_ms: aionui_common::now_ms() + UNUSED_TTL.as_millis() as i64,
            expires_at: now + UNUSED_TTL,
        };
        batch.grants = grants;
        for grant in &mut batch.grants {
            grant.deadline = prepared.expires_at;
        }
        batch.result.send_replace(Some(Ok(prepared)));
        true
    }

    async fn expire(
        binding: Weak<Self>,
        key: String,
        record: Weak<Mutex<Reservation>>,
        mut lifetime: watch::Receiver<Option<PreparationResult>>,
    ) {
        loop {
            let (deadline, browser) = {
                let Some(binding) = binding.upgrade() else {
                    return;
                };
                let mut inner = binding.0.lock().unwrap();
                inner.prune(Instant::now());
                let Some(batch) = inner.batches.get(&key) else {
                    return;
                };
                if record.as_ptr() != Arc::as_ptr(&batch.reservation) {
                    return;
                }
                let deadline = batch.grants.iter().map(|grant| grant.deadline).min().unwrap();
                let browser = batch
                    .admission
                    .browser
                    .clone()
                    .filter(|_| batch.grants.iter().any(|grant| grant.bytes.is_some()));
                (deadline, browser)
            };
            let recheck = Instant::now() + Duration::from_secs(1);
            let next = if browser.is_some() {
                deadline.min(recheck)
            } else {
                deadline
            };
            tokio::select! {
                change = lifetime.changed() => {
                    if change.is_err() {
                        return;
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(next) => {}
            }
            if let Some(browser) = browser
                && next == recheck
                && tokio::time::timeout_at(deadline.min(recheck + Duration::from_secs(1)), browser.recheck()).await
                    == Ok(false)
                && let Some(binding) = binding.upgrade()
            {
                binding
                    .0
                    .lock()
                    .unwrap()
                    .batches
                    .retain(|_, batch| record.as_ptr() != Arc::as_ptr(&batch.reservation));
                return;
            }
        }
    }

    pub(super) fn authorization(&self, grant: &str) -> Option<Arc<RelayWriteAdmission>> {
        let mut binding = self.0.lock().unwrap();
        binding.prune(Instant::now());
        Some(binding.batch(grant)?.admission.clone())
    }

    pub(super) fn revoke(&self, grant: &str) {
        let mut binding = self.0.lock().unwrap();
        binding
            .batches
            .retain(|_, batch| !batch.grants.iter().any(|item| item.descriptor.id == grant));
    }

    pub(super) fn try_reply(
        &self,
        request: &ReadRequest,
        id: &str,
        first_write: impl FnOnce(&str) -> std::task::Poll<std::io::Result<usize>>,
    ) -> Option<(String, std::task::Poll<std::io::Result<usize>>)> {
        let mut binding = self.0.lock().unwrap();
        let now = Instant::now();
        binding.prune(now);
        let batch = binding.batch(&request.reference)?;
        let grant = batch
            .grants
            .iter_mut()
            .find(|grant| grant.descriptor.id == request.reference)?;
        let (mut line, action) = grant.reply(request, id)?;
        line.push('\n');
        let result = first_write(&line);
        if matches!(result, std::task::Poll::Ready(Ok(written)) if written > 0) && grant.commit(action, request, now) {
            batch.account_bytes();
            batch.result.send_modify(|_| {});
        }
        Some((line, result))
    }
}

impl Binding {
    fn batch(&mut self, grant: &str) -> Option<&mut Batch> {
        self.batches
            .values_mut()
            .find(|batch| batch.grants.iter().any(|item| item.descriptor.id == grant))
    }

    fn prune(&mut self, now: Instant) {
        self.batches.retain(|_, batch| {
            if batch.admission.browser.as_ref().is_some_and(|browser| !browser.valid()) {
                return false;
            }
            if batch.result.borrow().is_none() {
                return true;
            }
            let count = batch.grants.len();
            batch.grants.retain(|grant| grant.deadline > now);
            if batch.grants.len() != count {
                batch.account_bytes();
            }
            !batch.grants.is_empty()
        });
    }
}

impl Batch {
    fn account_bytes(&self) {
        self.reservation.lock().unwrap().settle(
            self.grants
                .iter()
                .filter_map(|grant| grant.bytes.as_ref())
                .map(Vec::len)
                .sum(),
        );
    }
}

fn make_grants(bytes: Vec<Vec<u8>>) -> Vec<Grant> {
    bytes
        .into_iter()
        .map(|bytes| Grant {
            descriptor: ByteGrantDescriptor {
                id: uuid::Uuid::new_v4().to_string(),
                byte_length: bytes.len(),
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
            bytes: Some(bytes),
            deadline: Instant::now(),
            reader: None,
        })
        .collect()
}

pub(super) struct ReadRequest {
    pub session_id: String,
    pub binding_id: String,
    pub reference: String,
    reader_id: String,
    offset: usize,
    action: ReadAction,
}

enum ReadAction {
    Read { max_bytes: usize },
    Finish,
}

impl ReadRequest {
    pub(super) fn parse(params: &serde_json::value::RawValue) -> Option<Self> {
        // The two operations have disjoint fields, including null and duplicate keys.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Params {
            session_id: String,
            binding_id: String,
            #[serde(rename = "ref")]
            reference: String,
            reader_id: String,
            offset: usize,
            #[serde(default, deserialize_with = "present")]
            max_bytes: Option<usize>,
            #[serde(default, deserialize_with = "present")]
            finish: Option<bool>,
        }
        fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
            T::deserialize(d).map(Some)
        }
        let p: Params = serde_json::from_str(params.get()).ok()?;
        let action = match (p.max_bytes, p.finish) {
            (Some(max_bytes), None) if max_bytes > 0 => ReadAction::Read { max_bytes },
            (None, Some(true)) => ReadAction::Finish,
            _ => return None,
        };
        if p.reader_id.is_empty() || p.reader_id.len() > 96 || p.reference.len() != 36 {
            return None;
        }
        Some(Self {
            session_id: p.session_id,
            binding_id: p.binding_id,
            reference: p.reference,
            reader_id: p.reader_id,
            offset: p.offset,
            action,
        })
    }
}

enum ReadCommit {
    Chunk(std::ops::Range<usize>),
    Finish,
}

impl Grant {
    fn reply(&self, request: &ReadRequest, id: &str) -> Option<(String, ReadCommit)> {
        if self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.id != request.reader_id)
        {
            return None;
        }
        let length = self.descriptor.byte_length;
        let offset = request.offset;
        match request.action {
            ReadAction::Finish => {
                let reader = self.reader.as_ref()?;
                if offset != length || reader.last.end != length {
                    return None;
                }
                Some((
                    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"released":true}}}}"#),
                    ReadCommit::Finish,
                ))
            }
            ReadAction::Read { max_bytes } => {
                let bytes = self.bytes.as_ref()?;
                let mut reply = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"offset":{offset},"byteLength":{length},"sha256":"{}","eof":"#,
                    self.descriptor.sha256,
                );
                // Leave headroom for a retry using any accepted JSON-RPC ID.
                let overhead = reply.len() + (MAX_ID_BYTES - id.len()) + r#"false,"data":""}}"#.len();
                let limit = ((FRAME_LIMIT - overhead) / 4) * 3;
                let count = match &self.reader {
                    Some(reader) if offset == reader.last.start => reader.last.len(),
                    Some(reader) if offset == reader.last.end && offset < length => {
                        (length - offset).min(max_bytes).min(MAX_CHUNK).min(limit)
                    }
                    None if offset == 0 => length.min(max_bytes).min(MAX_CHUNK).min(limit),
                    _ => return None,
                };
                if count > limit || (count == 0 && offset < length) {
                    return None;
                }
                let end = offset + count;
                reply.push_str(if end == length {
                    r#"true,"data":""#
                } else {
                    r#"false,"data":""#
                });
                reply.reserve_exact(count.div_ceil(3) * 4 + 3);
                STANDARD.encode_string(&bytes[offset..end], &mut reply);
                reply.push_str(r#""}}"#);
                Some((reply, ReadCommit::Chunk(offset..end)))
            }
        }
    }
    fn commit(&mut self, action: ReadCommit, request: &ReadRequest, now: Instant) -> bool {
        match action {
            ReadCommit::Finish => {
                if self.bytes.take().is_none() {
                    return false;
                }
                self.deadline = now + FINISHED_TTL;
                true
            }
            ReadCommit::Chunk(last) => {
                let first_read = self.reader.is_none();
                if first_read {
                    self.deadline = now + READ_TTL;
                }
                self.reader = Some(Reader {
                    id: request.reader_id.clone(),
                    last,
                });
                first_read
            }
        }
    }
}

pub(super) fn error_reply(id: &str, code: i32, message: &'static str) -> String {
    let id: Value = serde_json::from_str(id).unwrap();
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string()
}

#[cfg(test)]
#[path = "byte_grants_test.rs"]
mod tests;
