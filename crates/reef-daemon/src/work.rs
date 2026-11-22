//! The CPU time a piece of async work used, as opposed to the time it
//! waited: what the inbox's board paces its reads by (#372, ADR 0091), so
//! a read that waited for a busy repository thread or a lock does not
//! count as costly.

use std::cell::Cell;
use std::time::Duration;

use nix::time::{ClockId, clock_gettime};

tokio::task_local! {
    static ELSEWHERE: Cell<Duration>;
}

/// The calling thread's CPU time so far.
fn cpu() -> Duration {
    clock_gettime(ClockId::CLOCK_THREAD_CPUTIME_ID).map_or(Duration::ZERO, Duration::from)
}

/// Runs `f`, and returns with its output the CPU time it used: in its
/// polls, and in what it handed to another thread through [`on_thread`]
/// (`RepoHandle::call`, the blocking reads of `read::Reads`, [`blocking`])
/// and awaited. Time it waited (for a repository thread busy with other
/// jobs, a lock, a timer) is not counted, nor is work in a task it spawned.
pub(crate) async fn worked<F: Future>(f: F) -> (F::Output, Duration) {
    ELSEWHERE
        .scope(Cell::new(Duration::ZERO), async move {
            let mut f = std::pin::pin!(f);
            let mut polled = Duration::ZERO;
            let output = std::future::poll_fn(|cx| {
                let began = cpu();
                let poll = f.as_mut().poll(cx);
                polled += cpu().saturating_sub(began);
                poll
            })
            .await;
            (output, polled + ELSEWHERE.with(Cell::get))
        })
        .await
}

/// Runs `work` on this thread, and returns with its output the CPU time it
/// used, for the task that waits for it to [`count`].
pub(crate) fn on_thread<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let began = cpu();
    let output = work();
    (output, cpu().saturating_sub(began))
}

/// Counts `used`, CPU time another thread spent for the calling task,
/// toward the enclosing [`worked`], if any.
pub(crate) fn count(used: Duration) {
    let _ = ELSEWHERE.try_with(|total| total.set(total.get() + used));
}

/// [`tokio::task::spawn_blocking`], counted by [`worked`].
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let (output, used) = tokio::task::spawn_blocking(move || on_thread(work)).await?;
    count(used);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn spin(for_: Duration) {
        let began = Instant::now();
        while began.elapsed() < for_ {
            std::hint::black_box(());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_on_other_threads_counts_and_waiting_does_not() {
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let held = std::sync::Arc::clone(&lock).lock_owned().await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(held);
        });
        let ((), used) = worked(async {
            blocking(|| spin(Duration::from_millis(100))).await.unwrap();
            blocking(|| std::thread::sleep(Duration::from_millis(100)))
                .await
                .unwrap();
            let _guard = lock.lock().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let blocked =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(100)));
            blocked.await.unwrap();
        })
        .await;
        release.await.unwrap();
        assert!(used >= Duration::from_millis(90), "{used:?}");
        assert!(used < Duration::from_millis(150), "{used:?}");
    }
}
