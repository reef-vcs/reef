//! The GC timer (#95, ADR 0041): the daemon runs garbage collection by
//! itself every `gc.interval`, over every repository, through
//! [`super::gc::run_timed`].
//!
//! A run is due at the latest of `meta.last_gc_at + interval` (a last run
//! in the future counts as now), `start_delay` after the timer started, and
//! an in-memory floor: `interval` after the last timed run, or `busy_retry`
//! after the maintenance lock was found held. `meta.last_gc_at` is written
//! by timed runs and by manual full runs, so a restart does not reset the
//! clock and `reef ws gc` postpones the next timed run.
//!
//! Due times are wall-clock times, checked at least every `check_every`:
//! tokio's clock (`Instant`) does not advance while a Mac sleeps, so a
//! single day-long sleep could take days on a laptop.
//!
//! The stop token ends the loop between runs. A run in progress is never
//! raced against it (only the sleep is): [`super::gc::run_timed`] stops at
//! a repository boundary by itself, and [`GcTimer::join`] waits for it.

use std::future::Future;
use std::sync::{Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::Task;
use super::gc;
use crate::daemon::AppState;

/// The timer's delays. Not configuration-file keys: tests set them on
/// [`crate::config::Config::gc_timer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The earliest a run starts after the timer does, keeping GC off the
    /// start path. Default 10 minutes.
    pub start_delay: Duration,
    /// How long a run found the maintenance lock held waits to try again.
    /// Default 10 minutes.
    pub busy_retry: Duration,
    /// The longest the loop sleeps before reading the wall clock again.
    /// Default 10 minutes.
    pub check_every: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            start_delay: Duration::from_mins(10),
            busy_retry: Duration::from_mins(10),
            check_every: Duration::from_mins(10),
        }
    }
}

/// What a run attempt came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ran {
    /// It ran (whatever each repository's outcome).
    Done,
    /// Another maintenance task held the lock; nothing ran.
    Busy,
}

/// What the loop runs and reads, so tests can drive it under a paused
/// clock with a wall clock of their own.
pub(crate) trait Runner: Send + Sync + 'static {
    /// The wall clock.
    fn now(&self) -> SystemTime;
    /// When the last full run ended (`meta.last_gc_at`).
    fn last_run(&self) -> Option<SystemTime>;
    /// One run attempt.
    fn run(&self) -> impl Future<Output = Ran> + Send;
}

/// How long from `now` until a run is due: at `last + interval` (a `last`
/// in the future counts as `now`), but not before `earliest`.
pub(crate) fn wait(
    interval: Duration,
    now: SystemTime,
    last: Option<SystemTime>,
    earliest: SystemTime,
) -> Duration {
    let due = match last {
        Some(last) => (last.min(now) + interval).max(earliest),
        None => earliest,
    };
    due.duration_since(now).unwrap_or(Duration::ZERO)
}

/// The timer's task, in a shutdown hook from before it starts.
#[derive(Debug, Default)]
pub struct GcTimer {
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl GcTimer {
    /// A timer not yet started.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the loop, which ends when `stop` is cancelled. Does nothing
    /// for a zero `interval` (the timer is off).
    pub(crate) fn start(
        &self,
        interval: Duration,
        limits: Limits,
        runner: impl Runner,
        stop: CancellationToken,
    ) {
        if interval.is_zero() {
            tracing::info!("GC timer off");
            return;
        }
        let handle = tokio::spawn(run_loop(interval, limits, runner, stop));
        *self.handle.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    /// Waits for the loop to end, a run in progress included; at once if
    /// it never started. Cancel its stop token first.
    pub async fn join(&self) {
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle
            && let Err(error) = handle.await
        {
            tracing::error!(%error, "the GC timer panicked");
        }
    }
}

async fn run_loop(
    interval: Duration,
    limits: Limits,
    runner: impl Runner,
    stop: CancellationToken,
) {
    let started = runner.now();
    let mut floor = started + limits.start_delay;
    tracing::info!(
        interval_ms = millis(interval),
        wait_ms = millis(wait(interval, started, runner.last_run(), floor)),
        "GC timer started"
    );
    loop {
        let now = runner.now();
        let until = wait(interval, now, runner.last_run(), floor);
        if !until.is_zero() {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                () = tokio::time::sleep(until.min(limits.check_every)) => continue,
            }
        }
        if stop.is_cancelled() {
            break;
        }
        // Never in a select: nothing may cut a run short.
        match runner.run().await {
            Ran::Done => floor = runner.now() + interval,
            Ran::Busy => {
                tracing::info!(
                    retry_ms = millis(limits.busy_retry),
                    "timed GC deferred: maintenance in progress"
                );
                floor = runner.now() + limits.busy_retry;
            }
        }
    }
    tracing::debug!("GC timer stopped");
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The daemon's [`Runner`]: the system clock, `meta.last_gc_at`, and
/// [`gc::run_timed`] under the maintenance lock. Holds the state weakly,
/// since the state holds the shutdown hook that holds the timer.
pub(crate) struct DaemonRunner(pub(crate) Weak<AppState>);

impl Runner for DaemonRunner {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn last_run(&self) -> Option<SystemTime> {
        let state = self.0.upgrade()?;
        gc::last_gc_at(&state).unwrap_or_else(|error| {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot read meta.last_gc_at");
            None
        })
    }

    async fn run(&self) -> Ran {
        let Some(state) = self.0.upgrade() else {
            return Ran::Done;
        };
        let Ok(_maintenance) = state.maintenance.try_acquire(Task::TimedGc) else {
            return Ran::Busy;
        };
        gc::run_timed(&state).await;
        Ran::Done
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Semaphore;
    use tokio::time::Instant;

    use super::*;

    const MIN: Duration = Duration::from_mins(1);
    const HOUR: Duration = Duration::from_hours(1);
    const DAY: Duration = Duration::from_hours(24);

    fn at(offset: Duration) -> SystemTime {
        SystemTime::UNIX_EPOCH + 1000 * DAY + offset
    }

    // 1
    #[test]
    fn wait_is_the_start_delay_without_a_last_run() {
        assert_eq!(wait(DAY, at(MIN), None, at(10 * MIN)), 9 * MIN);
    }

    // 2
    #[test]
    fn wait_counts_from_the_last_run_but_not_before_the_start_delay() {
        let now = at(3 * DAY);
        assert_eq!(wait(DAY, now, Some(now - 2 * HOUR), now), 22 * HOUR);
        assert_eq!(
            wait(DAY, now, Some(now - 3 * DAY), now + 10 * MIN),
            10 * MIN
        );
        assert_eq!(
            wait(DAY, now, Some(now - 3 * DAY), now - MIN),
            Duration::ZERO
        );
    }

    // 3
    #[test]
    fn a_last_run_in_the_future_counts_as_now() {
        let now = at(DAY);
        assert_eq!(wait(DAY, now, Some(now + 5 * DAY), now), DAY);
    }

    /// A wall clock that follows tokio's paused one, plus a jump the test
    /// sets; a run count; a number of `Busy` answers; and a gate a run
    /// waits on when set.
    struct Fake {
        base: Instant,
        jump: Mutex<Duration>,
        last: Mutex<Option<SystemTime>>,
        runs: AtomicUsize,
        busy: AtomicUsize,
        gate: Option<Arc<Semaphore>>,
    }

    impl Fake {
        fn new(last: Option<SystemTime>, gate: Option<Arc<Semaphore>>) -> Arc<Fake> {
            Arc::new(Fake {
                base: Instant::now(),
                jump: Mutex::new(Duration::ZERO),
                last: Mutex::new(last),
                runs: AtomicUsize::new(0),
                busy: AtomicUsize::new(0),
                gate,
            })
        }

        fn wall(&self) -> SystemTime {
            at(self.base.elapsed() + *self.jump.lock().unwrap())
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    impl Runner for Arc<Fake> {
        fn now(&self) -> SystemTime {
            self.wall()
        }

        fn last_run(&self) -> Option<SystemTime> {
            *self.last.lock().unwrap()
        }

        async fn run(&self) -> Ran {
            if self
                .busy
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Ran::Busy;
            }
            if let Some(gate) = &self.gate {
                gate.acquire().await.unwrap().forget();
            }
            self.runs.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(self.wall());
            Ran::Done
        }
    }

    fn start(fake: &Arc<Fake>) -> (Arc<GcTimer>, CancellationToken) {
        let timer = Arc::new(GcTimer::new());
        let stop = CancellationToken::new();
        timer.start(DAY, Limits::default(), Arc::clone(fake), stop.clone());
        (timer, stop)
    }

    /// Sleeps on the paused clock until `offset` past the fake's start.
    async fn until(fake: &Fake, offset: Duration) {
        tokio::time::sleep_until(fake.base + offset).await;
    }

    // 4
    #[tokio::test(start_paused = true)]
    async fn runs_after_the_start_delay_then_every_interval() {
        let fake = Fake::new(None, None);
        let (_timer, _stop) = start(&fake);
        until(&fake, 9 * MIN + 59 * Duration::from_secs(1)).await;
        assert_eq!(fake.runs(), 0);
        until(&fake, 10 * MIN + Duration::from_secs(1)).await;
        assert_eq!(fake.runs(), 1);
        until(&fake, DAY + 9 * MIN).await;
        assert_eq!(fake.runs(), 1);
        until(&fake, DAY + 11 * MIN).await;
        assert_eq!(fake.runs(), 2);
    }

    // 5
    #[tokio::test(start_paused = true)]
    async fn a_busy_lock_retries_after_busy_retry() {
        let fake = Fake::new(None, None);
        fake.busy.store(1, Ordering::SeqCst);
        let (_timer, _stop) = start(&fake);
        until(&fake, 11 * MIN).await;
        assert_eq!(fake.runs(), 0, "the first attempt found the lock held");
        until(&fake, 21 * MIN).await;
        assert_eq!(fake.runs(), 1);
    }

    // 6
    #[tokio::test(start_paused = true)]
    async fn a_slept_machine_runs_within_check_every() {
        let fake = Fake::new(Some(at(Duration::ZERO)), None);
        let (_timer, _stop) = start(&fake);
        until(&fake, Duration::from_secs(1)).await;
        // The machine slept 3 days; tokio's clock did not move meanwhile.
        *fake.jump.lock().unwrap() = 3 * DAY;
        until(&fake, 10 * MIN + Duration::from_secs(1)).await;
        assert_eq!(fake.runs(), 1);
    }

    // 7
    #[tokio::test(start_paused = true)]
    async fn a_manual_run_postpones_the_next() {
        let fake = Fake::new(None, None);
        let (_timer, _stop) = start(&fake);
        until(&fake, 12 * HOUR).await;
        assert_eq!(fake.runs(), 1);
        *fake.last.lock().unwrap() = Some(fake.wall());
        until(&fake, DAY + 11 * MIN).await;
        assert_eq!(fake.runs(), 1, "not due until a day after the manual run");
        until(&fake, DAY + 12 * HOUR + MIN).await;
        assert_eq!(fake.runs(), 2);
    }

    // 8
    #[tokio::test(start_paused = true)]
    async fn stop_waits_for_a_run_in_progress() {
        let gate = Arc::new(Semaphore::new(0));
        let fake = Fake::new(None, Some(Arc::clone(&gate)));
        let (timer, stop) = start(&fake);
        until(&fake, 11 * MIN).await;
        stop.cancel();
        let joined = tokio::spawn({
            let timer = Arc::clone(&timer);
            async move { timer.join().await }
        });
        until(&fake, 20 * MIN).await;
        assert!(!joined.is_finished(), "the run in progress holds it");
        gate.add_permits(1);
        joined.await.unwrap();
        assert_eq!(fake.runs(), 1);
        until(&fake, 2 * DAY).await;
        assert_eq!(fake.runs(), 1, "no run after the stop");
    }

    // 9
    #[tokio::test(start_paused = true)]
    async fn interval_zero_starts_nothing() {
        let fake = Fake::new(None, None);
        let timer = GcTimer::new();
        timer.start(
            Duration::ZERO,
            Limits::default(),
            Arc::clone(&fake),
            CancellationToken::new(),
        );
        assert!(timer.handle.lock().unwrap().is_none());
        timer.join().await;
        until(&fake, 2 * DAY).await;
        assert_eq!(fake.runs(), 0);
    }
}
