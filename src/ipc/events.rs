//! In-process event bus for IPC subscribers.
//!
//! The App emits lifecycle [`Event`]s (pane started / pane exited /
//! ...) via [`EventBus::emit`]. IPC subscriber connections register
//! via [`EventBus::subscribe`] (or [`EventBus::subscribe_scoped`] when
//! they want just one pane's inbox rather than the whole stream) and
//! stream the events out over the wire.
//!
//! # Routing (Issue #306)
//!
//! [`EventBus::emit`] broadcasts every [`Event`] to every live
//! subscriber, unchanged — with one **opt-in** narrowing.
//! [`Event::PeerInbox`] carries a `target_pane`, and a subscriber that
//! asked to be scoped to a pane ([`EventScope::PaneInbox`], bound from
//! the `from_pane` on its `Subscribe`) is enqueued only for the
//! `PeerInbox` addressed to that pane. A subscriber that asked for
//! nothing ([`EventScope::Unscoped`], the default) keeps receiving the
//! full stream — every event, including every `PeerInbox` whatever its
//! `target_pane` — exactly as it did before #306.
//!
//! Opting in is therefore purely additive: declining costs nothing and
//! changes nothing, which is what makes #306 a minor rather than a
//! break (`docs/semver-policy-2.0.md` §3: a new optional input whose
//! default preserves prior behavior).
//!
//! What opting in buys is **defense in depth**, not a boundary of any
//! kind. Any process running as this user can open the socket and claim
//! any pane id in its `Subscribe`, so the binding is not authentication
//! — see the threat model in [`crate::ipc`]. The gain is narrower: for
//! a scoped subscriber, a peer message meant for another pane is no
//! longer copied into its queue, which removes unintended delivery to
//! other panes and removes the queue pressure those copies caused.
//! Clients still compare `target_pane` against their own pane id as a
//! backstop, which is what keeps a new client talking to an old
//! (pre-#306, broadcast-only) server correct.
//!
//! # Delivery semantics (best-effort)
//!
//! Each subscriber has a bounded [`sync_channel`] of
//! [`CHANNEL_CAPACITY`] events. If a subscriber is too slow to
//! drain, new events are **dropped for that subscriber only** and
//! its [`EventBus`] will synthesize an [`Event::EventsDropped`]
//! meta-event on the next successful send so the subscriber can
//! recover awareness of the gap. We never block the App event loop.
//!
//! Events a subscriber's scope rejects — only ever another pane's
//! `PeerInbox`, and only for a subscriber that opted into a scope —
//! are not "dropped" in this sense: they are never offered to that
//! subscriber's channel in the first place, so they cannot fill it and
//! are not counted toward its [`Event::EventsDropped`] tally. That is
//! the concrete payoff of opting in.
//!
//! Because drops can happen, the event stream is **not** a reliable
//! replication source for a subscriber that needs an exact state
//! mirror. It's a live-feed for reactive workflows (e.g. "a worker
//! pane exited, react").
//!
//! Disconnected subscribers (their `Receiver` was dropped) are
//! cleaned up either (a) eagerly on [`EventBus::unsubscribe`] or
//! (b) lazily on the next [`EventBus::emit`] via `try_send`
//! detecting a disconnected sender. Note that (b) only observes
//! subscribers the emitted event's routing actually reaches, so a run
//! of `PeerInbox` events will not reclaim the slot of a *scoped*
//! subscriber they are not addressed to — the next lifecycle event
//! does. Unscoped subscribers are reached by every event, so they are
//! reclaimed as promptly as they always were.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use super::Event;

const CHANNEL_CAPACITY: usize = 256;

/// Opaque handle identifying an individual subscription. Used to
/// explicitly unregister without waiting for the next emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubId(u64);

/// Which slice of the event stream a subscriber receives.
///
/// The scope is fixed when the subscriber registers and is derived
/// from the `from_pane` field of its [`crate::ipc::Request::Subscribe`].
/// It exists so a subscriber can ask the server to route
/// [`Event::PeerInbox`] — the one event that is addressed to a
/// particular pane — instead of copying every pane's peer traffic into
/// its queue (Issue #306).
///
/// Scoping is **opt-in**, and the default preserves prior behavior. A
/// `Subscribe` that names no pane (every pre-#306 client, and `renga
/// events`) gets [`EventScope::Unscoped`] and keeps receiving the
/// whole stream — including every `PeerInbox`, whatever its
/// `target_pane` — byte for byte what it received before #306.
/// Declining to opt in costs nothing and changes nothing; that is what
/// keeps #306 a non-breaking minor.
///
/// What opting in buys, concretely: [`EventBus::emit`] never even
/// offers another pane's `PeerInbox` to a [`EventScope::PaneInbox`]
/// subscriber, so that traffic cannot occupy its bounded queue, cannot
/// push out events it does want, and cannot be delivered to it by
/// accident.
///
/// This is not an authentication mechanism, and it is not a boundary of
/// any kind. Any process running as this user can declare any pane id
/// (see the same-user trust model in [`crate::ipc`]); the value of the
/// binding is that well-behaved subscribers stop receiving — and stop
/// having to queue and discard — traffic meant for other panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventScope {
    /// Every event, including every [`Event::PeerInbox`] whatever its
    /// `target_pane`. The pre-#306 behavior, and what a `Subscribe`
    /// that names no pane still gets — which is why it is the default.
    #[default]
    Unscoped,
    /// Lifecycle events, plus only the [`Event::PeerInbox`] addressed
    /// to this pane.
    PaneInbox(usize),
}

/// Whether `scope` wants `event` delivered.
///
/// Kept as a free function (rather than inlined into
/// [`EventBus::emit`]) so the routing rule has exactly one definition
/// and can be asserted on directly in tests. The rule is a single
/// exception to broadcast: a [`EventScope::PaneInbox`] subscriber
/// takes an `Event::PeerInbox` only when the pane ids match.
/// **Everything else — every other event for a scoped subscriber, and
/// every event whatsoever for an [`EventScope::Unscoped`] one — is
/// accepted**, i.e. stays a broadcast.
fn scope_accepts(scope: EventScope, event: &Event) -> bool {
    match (scope, event) {
        (EventScope::PaneInbox(pane), Event::PeerInbox { target_pane, .. }) => *target_pane == pane,
        _ => true,
    }
}

struct Sub {
    id: SubId,
    tx: SyncSender<Event>,
    dropped_count: u64,
    scope: EventScope,
}

/// Multi-producer, multi-consumer event bus. Cheap to clone — the
/// internal subscriber list is `Arc`-shared.
#[derive(Default, Clone)]
pub struct EventBus {
    subs: Arc<Mutex<Vec<Sub>>>,
    next_id: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new subscriber for the full stream — i.e.
    /// [`EventScope::Unscoped`], which receives every event including
    /// every [`Event::PeerInbox`] regardless of `target_pane`. This is
    /// the pre-#306 behavior and stays the default. Use
    /// [`subscribe_scoped`](Self::subscribe_scoped) to opt into
    /// pane-scoped inbox routing instead.
    ///
    /// Returns the subscription id plus a receiver to drain events
    /// from. The caller should call [`unsubscribe`](Self::unsubscribe)
    /// when done; otherwise the bus will reclaim the slot lazily on the
    /// next emit.
    pub fn subscribe(&self) -> (SubId, Receiver<Event>) {
        self.subscribe_scoped(EventScope::Unscoped)
    }

    /// Register a new subscriber with an explicit [`EventScope`].
    ///
    /// The scope is bound here, once, from the pane the subscriber
    /// declared on its `Subscribe` line — it is never re-derived later
    /// from connection metadata, because nothing else on the
    /// connection (the `Hello` pid, a prior `PeerRegisterClient`) is
    /// tied to *this* subscription. A subscriber that declared no pane
    /// binds [`EventScope::Unscoped`] and is left on the full
    /// broadcast; the server never guesses a pane on its behalf.
    pub fn subscribe_scoped(&self, scope: EventScope) -> (SubId, Receiver<Event>) {
        let (tx, rx) = sync_channel(CHANNEL_CAPACITY);
        let id = SubId(self.next_id.fetch_add(1, Ordering::Relaxed));
        if let Ok(mut subs) = self.subs.lock() {
            subs.push(Sub {
                id,
                tx,
                dropped_count: 0,
                scope,
            });
        }
        (id, rx)
    }

    /// Explicitly remove a subscription. Safe to call even if the
    /// subscriber has already been GC'd by a previous emit.
    pub fn unsubscribe(&self, id: SubId) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.retain(|s| s.id != id);
        }
    }

    /// Deliver an event to the live subscribers that want it.
    ///
    /// This is a broadcast to all live subscribers for every [`Event`]
    /// variant, with one opt-in narrowing: [`Event::PeerInbox`] is
    /// routed for the subscribers that bound a pane, reaching only
    /// those whose pane equals its `target_pane` (see [`EventScope`]).
    /// Subscribers that bound no pane are unscoped and still receive
    /// it like anything else. Clients keep their own `target_pane`
    /// check as a backstop, so an old server that only ever broadcasts
    /// stays correct.
    ///
    /// A subscriber whose scope rejects the event is left completely
    /// untouched: no `try_send`, no drop-count increment, and no
    /// pending `EventsDropped` flush. Rejected events therefore cannot
    /// push a subscriber's queue toward overflow, and cannot inflate
    /// the gap count it is eventually told about — the property a
    /// subscriber opts in to gain.
    ///
    /// Among the subscribers that do want the event, delivery is
    /// best-effort: slow ones drop the event but stay subscribed
    /// (accumulating a count that is reported via a synthetic
    /// `EventsDropped` on the next successful send). Disconnected
    /// subscribers are removed.
    pub fn emit(&self, event: Event) {
        let mut subs = match self.subs.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        subs.retain_mut(|sub| {
            // Routing gate first, ahead of every side effect below: a
            // pane-scoped subscriber this event is not addressed to
            // must come out of `emit` bit-for-bit unchanged. Unscoped
            // subscribers always pass the gate and fall straight
            // through to the pre-#306 path.
            if !scope_accepts(sub.scope, &event) {
                return true;
            }
            // Then, flush any outstanding dropped-count notice.
            if sub.dropped_count > 0 {
                let notice = Event::EventsDropped {
                    count: sub.dropped_count,
                    ts_ms: now_ms(),
                };
                match sub.tx.try_send(notice) {
                    Ok(()) => {
                        sub.dropped_count = 0;
                    }
                    Err(TrySendError::Full(_)) => {
                        // Still too slow; defer the notice.
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        return false;
                    }
                }
            }
            match sub.tx.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    sub.dropped_count = sub.dropped_count.saturating_add(1);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.subs.lock().map(|s| s.len()).unwrap_or(0)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(id: usize) -> Event {
        Event::PaneStarted {
            id,
            name: None,
            role: None,
            ts_ms: 0,
        }
    }

    fn inbox(target_pane: usize) -> Event {
        Event::PeerInbox {
            target_pane,
            from_pane: 99,
            from_name: Some("sender".into()),
            from_kind: None,
            body: format!("hello pane {target_pane}"),
            ts_ms: 0,
        }
    }

    #[test]
    fn emit_reaches_single_subscriber() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        bus.emit(started(1));
        assert_eq!(rx.try_recv().ok(), Some(started(1)));
    }

    #[test]
    fn emit_fans_out_to_multiple_subscribers() {
        let bus = EventBus::new();
        let (_a, rx1) = bus.subscribe();
        let (_b, rx2) = bus.subscribe();
        bus.emit(started(7));
        assert_eq!(rx1.try_recv().ok(), Some(started(7)));
        assert_eq!(rx2.try_recv().ok(), Some(started(7)));
    }

    #[test]
    fn unsubscribe_removes_immediately() {
        let bus = EventBus::new();
        let (id, _rx) = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);
        bus.unsubscribe(id);
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn unsubscribe_of_unknown_id_is_noop() {
        let bus = EventBus::new();
        bus.unsubscribe(SubId(999));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[test]
    fn dropped_receiver_is_gc_on_next_emit() {
        let bus = EventBus::new();
        let (_a, rx1) = bus.subscribe();
        let (_b, rx2) = bus.subscribe();
        drop(rx2);
        assert_eq!(bus.subscriber_count(), 2);
        bus.emit(started(1));
        let _ = rx1.try_recv();
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn slow_subscriber_surfaces_events_dropped_meta_event() {
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        // Overflow the channel.
        for i in 0..(CHANNEL_CAPACITY + 5) {
            bus.emit(started(i));
        }
        // Drain the first window of events that fit.
        let mut payload_events = 0;
        while rx.try_recv().is_ok() {
            payload_events += 1;
        }
        assert!(payload_events <= CHANNEL_CAPACITY);
        assert!(payload_events > 0);

        // Next emit should prepend an EventsDropped meta-event with
        // the accumulated drop count, then the real event.
        bus.emit(started(9999));
        let first = rx.try_recv().expect("meta-event");
        match first {
            Event::EventsDropped { count, .. } => {
                assert!(count > 0, "expected non-zero drop count");
            }
            other => panic!("expected EventsDropped, got {other:?}"),
        }
        let second = rx.try_recv().expect("real event");
        assert_eq!(second, started(9999));
    }

    // ─── Issue #306: opt-in server-side PeerInbox routing ──────

    #[test]
    fn scope_accepts_every_non_inbox_event_for_every_scope() {
        for scope in [EventScope::Unscoped, EventScope::PaneInbox(3)] {
            assert!(scope_accepts(scope, &started(1)));
            assert!(scope_accepts(scope, &Event::Heartbeat { ts_ms: 0 }));
            assert!(scope_accepts(
                scope,
                &Event::EventsDropped { count: 2, ts_ms: 0 }
            ));
        }
    }

    #[test]
    fn a_pane_scope_accepts_inbox_only_for_the_matching_pane() {
        assert!(scope_accepts(EventScope::PaneInbox(3), &inbox(3)));
        assert!(!scope_accepts(EventScope::PaneInbox(3), &inbox(4)));
    }

    #[test]
    fn the_unscoped_default_accepts_a_peer_inbox_for_any_pane() {
        // The other half of the rule, and the one that makes #306
        // non-breaking: not opting into a pane changes nothing.
        assert_eq!(EventScope::default(), EventScope::Unscoped);
        for target in [0, 1, 3, 4, usize::MAX] {
            assert!(
                scope_accepts(EventScope::Unscoped, &inbox(target)),
                "unscoped rejected an inbox for pane {target}"
            );
        }
    }

    #[test]
    fn peer_inbox_reaches_only_the_subscriber_bound_to_its_target_pane() {
        // Both subscribers here opted in, so the routing rule is the
        // only thing under test. What an *unscoped* subscriber sees of
        // the same event is pinned separately, below.
        let bus = EventBus::new();
        let (_a, rx_a) = bus.subscribe_scoped(EventScope::PaneInbox(1));
        let (_b, rx_b) = bus.subscribe_scoped(EventScope::PaneInbox(2));

        bus.emit(inbox(1));

        assert_eq!(rx_a.try_recv().ok(), Some(inbox(1)));
        assert!(
            rx_b.try_recv().is_err(),
            "pane 2's subscriber must not see pane 1's inbox event"
        );
    }

    #[test]
    fn every_subscriber_bound_to_the_same_pane_receives_the_inbox_event() {
        // Two mcp-peer processes can legitimately watch the same pane
        // (e.g. a restarted subprocess whose predecessor has not been
        // reaped yet). Routing must fan out to all of them, not pick
        // one.
        let bus = EventBus::new();
        let (_a, rx_a) = bus.subscribe_scoped(EventScope::PaneInbox(5));
        let (_b, rx_b) = bus.subscribe_scoped(EventScope::PaneInbox(5));
        let (_c, rx_c) = bus.subscribe_scoped(EventScope::PaneInbox(5));

        bus.emit(inbox(5));

        assert_eq!(rx_a.try_recv().ok(), Some(inbox(5)));
        assert_eq!(rx_b.try_recv().ok(), Some(inbox(5)));
        assert_eq!(rx_c.try_recv().ok(), Some(inbox(5)));
    }

    #[test]
    fn an_unscoped_subscriber_still_receives_every_peer_inbox_as_before_306() {
        // The compatibility guarantee, and the reason #306 ships as a
        // minor: `renga events` and every pre-#306 client declare no
        // pane, land on the default scope, and must see exactly the
        // stream they saw before — every `PeerInbox`, for every pane,
        // in order. Scoping is opt-in; this is what declining costs.
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe();
        assert_eq!(
            EventScope::default(),
            EventScope::Unscoped,
            "`subscribe()` must keep meaning the full pre-#306 stream"
        );

        bus.emit(inbox(1));
        bus.emit(inbox(2));

        assert_eq!(rx.try_recv().ok(), Some(inbox(1)));
        assert_eq!(rx.try_recv().ok(), Some(inbox(2)));
        assert!(rx.try_recv().is_err(), "no extra events expected");
    }

    #[test]
    fn a_pane_scoped_and_an_unscoped_subscriber_split_on_the_same_peer_inbox() {
        // Both halves of the contract pinned against each other on a
        // single emit: one `PeerInbox`, addressed to a third pane that
        // is not the one the scoped subscriber bound, is withheld from
        // the subscriber that opted in and delivered to the one that
        // did not. Neither half can be tightened without breaking the
        // other.
        let bus = EventBus::new();
        let (_scoped, rx_scoped) = bus.subscribe_scoped(EventScope::PaneInbox(1));
        let (_unscoped, rx_unscoped) = bus.subscribe();

        bus.emit(inbox(3));

        assert!(
            rx_scoped.try_recv().is_err(),
            "a subscriber bound to pane 1 must not see pane 3's inbox event"
        );
        assert_eq!(
            rx_unscoped.try_recv().ok(),
            Some(inbox(3)),
            "an unscoped subscriber must still see it, as it did pre-#306"
        );
    }

    #[test]
    fn lifecycle_events_still_reach_scoped_and_unscoped_subscribers_alike() {
        let bus = EventBus::new();
        let (_unscoped, rx_unscoped) = bus.subscribe();
        let (_scoped, rx_scoped) = bus.subscribe_scoped(EventScope::PaneInbox(1));

        bus.emit(started(4));
        bus.emit(Event::PaneExited {
            id: 4,
            name: None,
            role: None,
            ts_ms: 0,
        });
        bus.emit(Event::Heartbeat { ts_ms: 7 });
        bus.emit(Event::EventsDropped { count: 1, ts_ms: 8 });

        for rx in [&rx_unscoped, &rx_scoped] {
            assert_eq!(rx.try_recv().ok(), Some(started(4)));
            assert!(matches!(rx.try_recv(), Ok(Event::PaneExited { id: 4, .. })));
            assert_eq!(rx.try_recv().ok(), Some(Event::Heartbeat { ts_ms: 7 }));
            assert_eq!(
                rx.try_recv().ok(),
                Some(Event::EventsDropped { count: 1, ts_ms: 8 })
            );
            assert!(rx.try_recv().is_err(), "no extra events expected");
        }
    }

    #[test]
    fn non_matching_peer_inbox_does_not_inflate_the_drop_count() {
        // The whole point of gating *before* `try_send`, and the
        // benefit a subscriber opts in to buy: events it asked not to
        // receive must not consume its queue budget, and must not show
        // up in the gap it is later told about. Deliberately a *bound*
        // subscriber — an unscoped one accepts every `PeerInbox`, so
        // for it there is nothing to reject and nothing to prove.
        let bus = EventBus::new();
        let (_id, rx) = bus.subscribe_scoped(EventScope::PaneInbox(1));

        // Fill the channel exactly to capacity — nothing dropped yet.
        for i in 0..CHANNEL_CAPACITY {
            bus.emit(started(i));
        }
        // Three genuine overflows.
        for i in 0..3 {
            bus.emit(started(10_000 + i));
        }
        // Ten events addressed to a different pane. These must be
        // ignored entirely rather than counted as drops.
        for _ in 0..10 {
            bus.emit(inbox(2));
        }

        let mut drained = 0;
        while rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, CHANNEL_CAPACITY, "capacity should have been full");

        bus.emit(started(20_000));
        match rx.try_recv().expect("meta-event") {
            Event::EventsDropped { count, .. } => {
                assert_eq!(count, 3, "expected 3 real drops, not the 10 rejected ones");
            }
            other => panic!("expected EventsDropped, got {other:?}"),
        }
        assert_eq!(rx.try_recv().ok(), Some(started(20_000)));
    }
}
