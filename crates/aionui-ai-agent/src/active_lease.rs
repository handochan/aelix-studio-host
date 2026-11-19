use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use aionui_common::{TimestampMs, now_ms};
use dashmap::DashMap;
use tokio::sync::{Mutex, OwnedMutexGuard};

pub const ACTIVE_LEASE_TTL_MS: TimestampMs = 90_000;
const MAX_TRACKED_VIEWS: usize = 128;

#[derive(Debug)]
struct ViewLease {
    expires_at: TimestampMs,
    departed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewDeparture {
    Inactive,
    ViewActive,
    LastView(u64),
}

#[derive(Debug, Default)]
struct ConversationLeases {
    legacy: TimestampMs,
    views: HashMap<String, ViewLease>,
    lifecycle: Arc<Mutex<()>>,
    pending_release: Option<u64>,
}

impl ConversationLeases {
    fn expire_views(&mut self, now: TimestampMs) {
        self.views.retain(|_, view| view.expires_at > now);
    }

    fn active_until(&self, now: TimestampMs) -> Option<TimestampMs> {
        self.views
            .values()
            .filter(|view| !view.departed)
            .map(|view| view.expires_at)
            .chain([self.legacy])
            .max()
            .filter(|expiry| *expiry > now)
    }
}

#[derive(Debug)]
pub struct ActiveLeaseRegistry {
    leases: DashMap<String, ConversationLeases>,
    ttl_ms: TimestampMs,
    next_departure: AtomicU64,
    next_sweep_ms: AtomicI64,
}

impl ActiveLeaseRegistry {
    pub fn new() -> Self {
        Self::with_ttl_ms(ACTIVE_LEASE_TTL_MS)
    }

    pub fn with_ttl_ms(ttl_ms: TimestampMs) -> Self {
        Self {
            leases: DashMap::new(),
            ttl_ms,
            next_departure: AtomicU64::new(1),
            next_sweep_ms: AtomicI64::new(0),
        }
    }

    pub fn renew(&self, conversation_id: &str) -> TimestampMs {
        let expires_at = now_ms().saturating_add(self.ttl_ms);
        self.leases.entry(conversation_id.to_owned()).or_default().legacy = expires_at;
        expires_at
    }

    /// Serialize open, lease changes and explicit close for this conversation.
    pub async fn lock_conversation(&self, conversation_id: &str) -> OwnedMutexGuard<()> {
        self.prune_expired();
        let lock = Arc::clone(&self.leases.entry(conversation_id.to_owned()).or_default().lifecycle);
        lock.lock_owned().await
    }

    /// Named view transitions run while the conversation lifecycle lock is held.
    pub fn register_view(&self, conversation_id: &str, view_id: &str) -> bool {
        let now = now_ms();
        let mut leases = self.leases.entry(conversation_id.to_owned()).or_default();
        leases.expire_views(now);
        match leases.views.get(view_id) {
            Some(view) if view.departed => return false,
            None if leases.views.len() >= MAX_TRACKED_VIEWS => return false,
            _ => {}
        }
        leases.views.insert(
            view_id.to_owned(),
            ViewLease {
                expires_at: now.saturating_add(self.ttl_ms),
                departed: false,
            },
        );
        leases.pending_release = None;
        true
    }

    pub fn renew_view(&self, conversation_id: &str, view_id: &str) -> bool {
        let now = now_ms();
        let Some(mut leases) = self.leases.get_mut(conversation_id) else {
            return false;
        };
        let Some(view) = leases.views.get_mut(view_id) else {
            return false;
        };
        if view.departed || view.expires_at <= now {
            return false;
        }
        view.expires_at = now.saturating_add(self.ttl_ms);
        true
    }

    /// Only a live view can schedule a close. A short tombstone also fences a
    /// heartbeat or ensure that was already in flight when an unknown view left.
    pub fn depart_view(&self, conversation_id: &str, view_id: &str) -> ViewDeparture {
        let now = now_ms();
        let mut leases = self.leases.entry(conversation_id.to_owned()).or_default();
        leases.expire_views(now);
        let was_active = match leases.views.get(view_id) {
            Some(view) if view.departed => return ViewDeparture::Inactive,
            Some(_) => true,
            None if leases.views.len() >= MAX_TRACKED_VIEWS => return ViewDeparture::Inactive,
            None => false,
        };
        leases.views.insert(
            view_id.to_owned(),
            ViewLease {
                expires_at: now.saturating_add(self.ttl_ms),
                departed: true,
            },
        );
        if !was_active {
            return ViewDeparture::Inactive;
        }
        if leases.active_until(now).is_some() {
            return ViewDeparture::ViewActive;
        }
        let departure = self.next_departure.fetch_add(1, Ordering::Relaxed);
        leases.pending_release = Some(departure);
        ViewDeparture::LastView(departure)
    }

    pub fn release_pending(&self, conversation_id: &str, departure: u64) -> bool {
        self.leases
            .get(conversation_id)
            .is_some_and(|leases| leases.pending_release == Some(departure) && leases.active_until(now_ms()).is_none())
    }

    pub fn finish_release(&self, conversation_id: &str, departure: u64) {
        if let Some(mut leases) = self.leases.get_mut(conversation_id)
            && leases.pending_release == Some(departure)
        {
            leases.pending_release = None;
        }
    }

    pub fn renew_many<'a>(&self, conversation_ids: impl IntoIterator<Item = &'a str>) -> (usize, TimestampMs) {
        let expires_at = now_ms().saturating_add(self.ttl_ms);
        let mut count = 0;
        for conversation_id in conversation_ids {
            self.leases.entry(conversation_id.to_owned()).or_default().legacy = expires_at;
            count += 1;
        }
        (count, expires_at)
    }

    fn prune_expired(&self) {
        let now = now_ms();
        let next = self.next_sweep_ms.load(Ordering::Relaxed);
        if now >= next
            && self
                .next_sweep_ms
                .compare_exchange(
                    next,
                    now.saturating_add(self.ttl_ms),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            self.leases.retain(|_, leases| !Self::is_expired(leases, now));
        }
    }

    fn is_expired(leases: &mut ConversationLeases, now: TimestampMs) -> bool {
        leases.expire_views(now);
        leases.active_until(now).is_none()
            && leases.views.is_empty()
            && leases.pending_release.is_none()
            && Arc::strong_count(&leases.lifecycle) == 1
    }

    pub fn active_until(&self, conversation_id: &str) -> Option<TimestampMs> {
        let now = now_ms();
        let mut expires_at = None;
        self.leases.remove_if_mut(conversation_id, |_, leases| {
            expires_at = leases.active_until(now);
            Self::is_expired(leases, now)
        });
        expires_at
    }

    pub fn is_active(&self, conversation_id: &str) -> bool {
        self.active_until(conversation_id).is_some()
    }
}

impl Default for ActiveLeaseRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn renew_sets_active_lease() {
        let registry = ActiveLeaseRegistry::new();

        let expires_at = registry.renew("conv-1");

        assert_eq!(registry.active_until("conv-1"), Some(expires_at));
        assert!(registry.is_active("conv-1"));
    }

    #[test]
    fn renew_many_sets_all_leases_and_returns_count() {
        let registry = ActiveLeaseRegistry::new();

        let (count, expires_at) = registry.renew_many(["conv-1", "conv-2"]);

        assert_eq!(count, 2);
        assert_eq!(registry.active_until("conv-1"), Some(expires_at));
        assert_eq!(registry.active_until("conv-2"), Some(expires_at));
    }

    #[test]
    fn departure_is_single_use_and_a_new_view_supersedes_pending_close() {
        let registry = ActiveLeaseRegistry::new();
        assert!(registry.register_view("conv", "a"));
        let ViewDeparture::LastView(pending) = registry.depart_view("conv", "a") else {
            panic!("the last view should schedule a release");
        };
        assert!(registry.release_pending("conv", pending));
        assert!(!registry.renew_view("conv", "a"));
        assert!(registry.register_view("conv", "b"));
        assert_eq!(registry.depart_view("conv", "a"), ViewDeparture::Inactive);
        assert!(!registry.release_pending("conv", pending));
        assert!(registry.is_active("conv"));
    }

    #[test]
    fn unknown_departure_cannot_close_and_fences_a_late_open() {
        let registry = ActiveLeaseRegistry::new();
        assert_eq!(registry.depart_view("conv", "a"), ViewDeparture::Inactive);
        assert!(!registry.register_view("conv", "a"));
        assert!(!registry.renew_view("conv", "a"));
        assert!(!registry.is_active("conv"));
    }

    #[test]
    fn closing_one_of_two_active_views_preserves_the_other_view() {
        let registry = ActiveLeaseRegistry::new();
        registry.register_view("conv", "a");
        registry.register_view("conv", "b");
        assert_eq!(registry.depart_view("conv", "a"), ViewDeparture::ViewActive);
        assert!(matches!(registry.depart_view("conv", "b"), ViewDeparture::LastView(_)));
    }

    #[test]
    fn expired_view_cannot_authorize_close_or_revive_by_heartbeat() {
        let registry = ActiveLeaseRegistry::with_ttl_ms(1);
        registry.register_view("conv", "a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(!registry.renew_view("conv", "a"));
        assert_eq!(registry.depart_view("conv", "a"), ViewDeparture::Inactive);
    }

    #[test]
    fn active_lookup_returns_none_for_missing_lease() {
        let registry = ActiveLeaseRegistry::new();

        assert_eq!(registry.active_until("missing"), None);
        assert!(!registry.is_active("missing"));
    }

    #[test]
    fn active_lookup_lazily_removes_expired_lease() {
        let registry = ActiveLeaseRegistry::with_ttl_ms(1);
        registry.renew("conv-1");

        std::thread::sleep(Duration::from_millis(5));

        assert_eq!(registry.active_until("conv-1"), None);
        assert!(!registry.leases.contains_key("conv-1"));
    }
}
