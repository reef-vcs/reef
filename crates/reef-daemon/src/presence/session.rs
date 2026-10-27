//! The task behind each `SubscribePresence` stream.
//!
//! It sends the current set, then drains the hub's broadcast into a map of
//! pending states keyed by entry, latest state wins, whether or not the
//! transport has taken the last frame. It builds a frame only once the
//! channel of one has room, so a slow client costs one built frame and at
//! most one pending state per entry it follows. A session that falls
//! behind the broadcast resynchronizes from the hub's current set.

use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use prost::Message as _;
use reef_api::v1::{PresenceEndReason, SubscribePresenceResponse};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use super::hub::{Change, PresenceHub};
use super::{Limits, PresenceError, PresenceFilter};
use crate::error::status;
use crate::model::PresenceId;
use crate::subscribe::Slot;

/// One frame, or the error that ends the stream.
pub(crate) type Frame = Result<SubscribePresenceResponse, Status>;

/// Starts the session's task; its frames arrive on the receiver.
pub(crate) fn spawn(
    hub: Arc<PresenceHub>,
    filter: PresenceFilter,
    shutdown: CancellationToken,
    slot: Slot,
) -> mpsc::Receiver<Frame> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let _slot = slot;
        let limits = hub.limits().clone();
        let mut session = Session {
            hub,
            filter,
            limits,
            shutdown,
            sent: HashMap::new(),
            pending: BTreeMap::new(),
        };
        let served = AssertUnwindSafe(session.serve(&tx)).catch_unwind().await;
        let error = match served {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error,
            Err(_) => {
                tracing::error!("presence subscription task panicked");
                Status::internal("the subscription failed")
            }
        };
        // The client may not be reading; the stream ends either way.
        let _ = tokio::time::timeout(Duration::from_secs(1), tx.send(Err(error))).await;
    });
    rx
}

struct Session {
    hub: Arc<PresenceHub>,
    filter: PresenceFilter,
    limits: Limits,
    shutdown: CancellationToken,
    /// Live entries the client was sent, at the state it was sent.
    sent: HashMap<PresenceId, Arc<Change>>,
    /// States to send, one per entry.
    pending: BTreeMap<PresenceId, Arc<Change>>,
}

impl Session {
    /// Sends frames until the client goes away (`Ok`) or the stream must
    /// end with an error.
    async fn serve(&mut self, tx: &mpsc::Sender<Frame>) -> Result<(), Status> {
        let (set, mut changes) = self.hub.subscribe(&self.filter);
        for change in set {
            self.pending.insert(change.id, change);
        }
        // The set: frames back to back, the last one synced, one empty
        // synced frame when there is nothing.
        let mut set_events = false;
        loop {
            let permit = self.reserve(tx, &mut changes, false).await?;
            let Some(permit) = permit else {
                return Ok(());
            };
            let (events, more) = self.cut();
            set_events |= !events.is_empty();
            permit.send(Ok(SubscribePresenceResponse {
                events,
                synced: !more,
            }));
            if !more {
                break;
            }
        }
        let mut last_frame = Instant::now();
        let mut last_events = set_events.then_some(last_frame);
        let mut cut = false;
        loop {
            self.drain(&mut changes);
            let mut keepalive = false;
            if self.pending.is_empty() {
                tokio::select! {
                    biased;
                    () = self.shutdown.cancelled() => return Err(shutting_down()),
                    () = tx.closed() => return Ok(()),
                    received = changes.recv() => {
                        self.take(received, &mut changes)?;
                        continue;
                    }
                    () = tokio::time::sleep_until(last_frame + self.limits.keepalive) => {
                        keepalive = true;
                    }
                }
            }
            // Frames with events are spaced; everything changed meanwhile
            // goes in the next one. The rest of a frame a bound cut goes at
            // once.
            if !keepalive
                && !cut
                && let Some(at) = last_events
            {
                let next = at + self.limits.frame_interval;
                while Instant::now() < next {
                    tokio::select! {
                        biased;
                        () = self.shutdown.cancelled() => return Err(shutting_down()),
                        () = tx.closed() => return Ok(()),
                        received = changes.recv() => self.take(received, &mut changes)?,
                        () = tokio::time::sleep_until(next) => {}
                    }
                }
            }
            let Some(permit) = self.reserve(tx, &mut changes, true).await? else {
                return Ok(());
            };
            self.drain(&mut changes);
            let (events, more) = self.cut();
            if events.is_empty() && !keepalive {
                // Everything pending was superseded or dropped meanwhile.
                drop(permit);
                cut = false;
                continue;
            }
            let carried = !events.is_empty();
            permit.send(Ok(SubscribePresenceResponse {
                events,
                synced: true,
            }));
            last_frame = Instant::now();
            if carried {
                last_events = Some(last_frame);
            }
            cut = more;
        }
    }

    /// Waits for room in the channel, taking changes meanwhile; `None` when
    /// the client went away.
    async fn reserve<'a>(
        &mut self,
        tx: &'a mpsc::Sender<Frame>,
        changes: &mut broadcast::Receiver<Arc<Change>>,
        take: bool,
    ) -> Result<Option<mpsc::Permit<'a, Frame>>, Status> {
        loop {
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => return Err(shutting_down()),
                permit = tx.reserve() => return Ok(permit.ok()),
                received = changes.recv(), if take => self.take(received, changes)?,
            }
        }
    }

    /// Takes every change already broadcast.
    fn drain(&mut self, changes: &mut broadcast::Receiver<Arc<Change>>) {
        loop {
            match changes.try_recv() {
                Ok(change) => self.pend(change),
                Err(broadcast::error::TryRecvError::Lagged(_)) => self.resync(),
                Err(_) => return,
            }
        }
    }

    /// Takes one received change.
    fn take(
        &mut self,
        received: Result<Arc<Change>, broadcast::error::RecvError>,
        changes: &mut broadcast::Receiver<Arc<Change>>,
    ) -> Result<(), Status> {
        match received {
            Ok(change) => self.pend(change),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                self.resync();
                self.drain(changes);
            }
            Err(broadcast::error::RecvError::Closed) => return Err(shutting_down()),
        }
        Ok(())
    }

    /// Holds `change` to send unless the client has it or a newer state of
    /// its entry is pending; an entry that ends before it was sent is not
    /// sent at all.
    fn pend(&mut self, change: Arc<Change>) {
        if !self.filter.matches(&change) {
            return;
        }
        let id = change.id;
        let known = self
            .pending
            .get(&id)
            .or_else(|| self.sent.get(&id))
            .map(|known| known.seq);
        if known.is_some_and(|seq| seq >= change.seq) {
            return;
        }
        if change.end.is_some() && !self.sent.contains_key(&id) {
            self.pending.remove(&id);
            return;
        }
        self.pending.insert(id, change);
    }

    /// Repairs a lag from the hub's current set: what is new or newer is
    /// pending, and every entry the client holds that is gone ends, with
    /// its reason if the hub still knows it.
    fn resync(&mut self) {
        tracing::debug!("presence subscription fell behind; resynchronizing");
        let current: HashMap<PresenceId, Arc<Change>> = self
            .hub
            .current(&self.filter)
            .into_iter()
            .map(|change| (change.id, change))
            .collect();
        self.pending
            .retain(|id, change| change.end.is_some() || current.contains_key(id));
        for change in current.values() {
            self.pend(Arc::clone(change));
        }
        let gone: Vec<Arc<Change>> = self
            .sent
            .iter()
            .filter(|(id, _)| !current.contains_key(id))
            .map(|(_, change)| Arc::clone(change))
            .collect();
        for change in gone {
            if self
                .pending
                .get(&change.id)
                .is_some_and(|p| p.end.is_some())
            {
                continue;
            }
            let reason = self
                .hub
                .ended(change.id)
                .unwrap_or(PresenceEndReason::Unspecified);
            // Newer than anything the client has of it.
            let seq = change.seq.saturating_add(1);
            self.pending
                .insert(change.id, Arc::new(change.ended(seq, reason)));
        }
    }

    /// Takes pending states into one frame's events, in id order and
    /// within its bounds, and records them as sent; whether any is left.
    fn cut(&mut self) -> (Vec<reef_api::v1::PresenceEvent>, bool) {
        let mut events = Vec::new();
        let mut bytes = 0;
        while events.len() < self.limits.frame_events.max(1) {
            let Some(entry) = self.pending.first_entry() else {
                break;
            };
            let event = entry.get().event();
            let size = event.encoded_len() + 8;
            if !events.is_empty() && bytes + size > self.limits.frame_bytes {
                break;
            }
            let change = entry.remove();
            bytes += size;
            if change.end.is_some() {
                self.sent.remove(&change.id);
            } else {
                self.sent.insert(change.id, change);
            }
            events.push(event);
        }
        (events, !self.pending.is_empty())
    }
}

fn shutting_down() -> Status {
    status(PresenceError::ShuttingDown)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use reef_api::v1::PresenceKind;

    use super::*;
    use crate::presence::Report;
    use crate::subscribe::Subscriptions;

    fn activity(agent: &str, text: &str) -> Report {
        Report {
            agent: agent.to_owned(),
            kind: PresenceKind::Activity,
            activity: text.to_owned(),
            ttl: Duration::from_secs(60),
            ..Report::default()
        }
    }

    struct Fixture {
        hub: Arc<PresenceHub>,
        stop: CancellationToken,
        subscriptions: Subscriptions,
    }

    impl Fixture {
        fn new(limits: Limits) -> Fixture {
            let hub = Arc::new(PresenceHub::new(limits));
            let stop = CancellationToken::new();
            hub.start(stop.clone());
            Fixture {
                hub,
                stop,
                subscriptions: Subscriptions::default(),
            }
        }

        fn subscribe(&self, filter: PresenceFilter) -> mpsc::Receiver<Frame> {
            let slot = self.subscriptions.admit(10).unwrap();
            spawn(Arc::clone(&self.hub), filter, self.stop.clone(), slot)
        }
    }

    async fn frame(rx: &mut mpsc::Receiver<Frame>) -> SubscribePresenceResponse {
        rx.recv().await.unwrap().unwrap()
    }

    fn activities(frame: &SubscribePresenceResponse) -> Vec<(String, bool)> {
        frame
            .events
            .iter()
            .map(|event| {
                (
                    event.presence.as_ref().unwrap().activity.clone(),
                    event.ended,
                )
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_set_is_one_synced_frame() {
        let fixture = Fixture::new(Limits::default());
        let mut rx = fixture.subscribe(PresenceFilter::default());
        let first = frame(&mut rx).await;
        assert_eq!(first.events.len(), 0);
        assert!(first.synced);
    }

    #[tokio::test(start_paused = true)]
    async fn the_set_is_cut_in_id_order_and_synced_last() {
        let fixture = Fixture::new(Limits {
            frame_events: 2,
            ..Limits::default()
        });
        let mut ids = Vec::new();
        for n in 0..3 {
            let presence = fixture
                .hub
                .report(None, activity("a", &n.to_string()))
                .unwrap();
            ids.push(presence.id);
        }
        ids.sort();
        let mut rx = fixture.subscribe(PresenceFilter::default());
        let first = frame(&mut rx).await;
        let second = frame(&mut rx).await;
        assert_eq!(first.events.len(), 2);
        assert!(!first.synced);
        assert_eq!(second.events.len(), 1);
        assert!(second.synced);
        let got: Vec<String> = first
            .events
            .iter()
            .chain(&second.events)
            .map(|event| event.presence.as_ref().unwrap().id.clone())
            .collect();
        assert_eq!(got, ids);
    }

    #[tokio::test(start_paused = true)]
    async fn the_latest_state_wins_and_unsent_ends_vanish() {
        let fixture = Fixture::new(Limits {
            publish_interval: Duration::ZERO,
            frame_interval: Duration::from_millis(200),
            ..Limits::default()
        });
        let mut rx = fixture.subscribe(PresenceFilter::default());
        assert!(frame(&mut rx).await.synced);
        let kept = fixture.hub.report(None, activity("a", "0")).unwrap();
        let first = frame(&mut rx).await;
        assert_eq!(
            activities(&first),
            [("0".to_owned(), false)],
            "isolated: at once"
        );
        let kept: PresenceId = kept.id.parse().unwrap();
        for n in 1..=5 {
            fixture
                .hub
                .report(Some(kept), activity("a", &n.to_string()))
                .unwrap();
        }
        let brief = fixture.hub.report(None, activity("a", "brief")).unwrap();
        fixture.hub.clear(brief.id.parse().unwrap()).unwrap();
        let next = frame(&mut rx).await;
        assert_eq!(activities(&next), [("5".to_owned(), false)]);
        fixture.hub.clear(kept).unwrap();
        let end = frame(&mut rx).await;
        assert_eq!(activities(&end), [("5".to_owned(), true)]);
        assert_eq!(
            end.events[0].end_reason,
            i32::from(PresenceEndReason::Cleared)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_keepalive_after_the_interval() {
        let fixture = Fixture::new(Limits::default());
        let mut rx = fixture.subscribe(PresenceFilter::default());
        frame(&mut rx).await;
        let started = Instant::now();
        let keepalive = frame(&mut rx).await;
        assert_eq!(keepalive.events.len(), 0);
        assert!(keepalive.synced);
        assert!(started.elapsed() >= Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lagging_session_resynchronizes() {
        let fixture = Fixture::new(Limits {
            publish_interval: Duration::ZERO,
            backlog: 4,
            ..Limits::default()
        });
        let mut ids = Vec::new();
        for n in 0..3 {
            let presence = fixture
                .hub
                .report(None, activity("a", &format!("{n}")))
                .unwrap();
            ids.push(presence.id.parse::<PresenceId>().unwrap());
        }
        let mut session = Session {
            hub: Arc::clone(&fixture.hub),
            filter: PresenceFilter::default(),
            limits: fixture.hub.limits().clone(),
            shutdown: fixture.stop.clone(),
            sent: HashMap::new(),
            pending: BTreeMap::new(),
        };
        let (set, mut changes) = fixture.hub.subscribe(&session.filter);
        for change in set {
            session.pending.insert(change.id, change);
        }
        session.cut();
        assert_eq!(session.sent.len(), 3);
        // More changes than the backlog holds: one cleared, one replaced
        // many times, one new.
        fixture.hub.clear(ids[0]).unwrap();
        for n in 0..10 {
            fixture
                .hub
                .report(Some(ids[1]), activity("a", &format!("r{n}")))
                .unwrap();
        }
        fixture.hub.report(None, activity("a", "new")).unwrap();
        session.drain(&mut changes);
        let mut got: Vec<(String, bool, i32)> = session
            .pending
            .values()
            .map(|change| {
                let event = change.event();
                (
                    event.presence.unwrap().activity,
                    event.ended,
                    event.end_reason,
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("0".to_owned(), true, i32::from(PresenceEndReason::Cleared)),
                ("new".to_owned(), false, 0),
                ("r9".to_owned(), false, 0),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn filters_apply_to_the_set_and_changes() {
        let fixture = Fixture::new(Limits::default());
        fixture.hub.report(None, activity("a", "a0")).unwrap();
        fixture.hub.report(None, activity("b", "b0")).unwrap();
        let mut rx = fixture.subscribe(PresenceFilter {
            agents: Some(HashSet::from(["a".to_owned()])),
            ..PresenceFilter::default()
        });
        let set = frame(&mut rx).await;
        assert_eq!(activities(&set), [("a0".to_owned(), false)]);
        fixture.hub.report(None, activity("b", "b1")).unwrap();
        fixture.hub.report(None, activity("a", "a1")).unwrap();
        let next = frame(&mut rx).await;
        assert_eq!(activities(&next), [("a1".to_owned(), false)]);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_ends_the_stream_without_a_reason() {
        let fixture = Fixture::new(Limits::default());
        let mut rx = fixture.subscribe(PresenceFilter::default());
        frame(&mut rx).await;
        fixture.stop.cancel();
        let error = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert!(
            reef_api::error::detail(&error).is_none_or(|detail| detail.reason.is_empty()),
            "no reason"
        );
    }
}
