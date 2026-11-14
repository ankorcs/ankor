//! The lock every module takes, and the runs that callers share.

use std::sync::{Mutex, MutexGuard, PoisonError};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

/// A lock that a panic left poisoned is taken as it is. The release build aborts on a panic, so only
/// tests and debug builds can meet a poisoned lock. There the state is still the last one a holder
/// left.
#[allow(clippy::disallowed_methods, reason = "the one place the agent locks a mutex")]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A run that every caller joins until it ends.
pub(crate) type SharedRun<T> = Shared<BoxFuture<'static, T>>;

/// Joins the run kept in `slot`, or starts `run` there. A run this call starts is spawned once
/// `state` is released, so it ends even when every caller stops waiting.
///
/// `run` empties its slot itself, in the hold that applies its result, so a caller that arrives
/// after the result starts a new run.
pub(crate) fn join_or_spawn<S, T>(
    state: &Mutex<S>,
    slot: impl FnOnce(&mut S) -> &mut Option<SharedRun<T>>,
    run: impl FnOnce() -> BoxFuture<'static, T>,
) -> SharedRun<T>
where
    T: Clone + Send + Sync + 'static,
{
    let mut state = lock(state);
    let slot = slot(&mut state);

    if let Some(running) = slot.as_ref() {
        return running.clone();
    }

    let running = run().shared();

    *slot = Some(running.clone());
    drop(state);
    tokio::spawn(running.clone());
    running
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct State {
        running: Option<SharedRun<usize>>,
    }

    fn start(state: &Arc<Mutex<State>>, runs: &Arc<AtomicUsize>, gate: &Arc<tokio::sync::Semaphore>) -> SharedRun<usize> {
        join_or_spawn(
            state,
            |state| &mut state.running,
            || {
                let (state, runs, gate) = (Arc::clone(state), Arc::clone(runs), Arc::clone(gate));

                async move {
                    let run = runs.fetch_add(1, Ordering::SeqCst) + 1;
                    let _open = gate.acquire().await;

                    lock(&state).running = None;
                    run
                }
                .boxed()
            },
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn callers_from_several_threads_join_one_run() {
        let (state, runs, gate) = (Arc::default(), Arc::default(), Arc::new(tokio::sync::Semaphore::new(0)));
        let joined = Arc::new(AtomicUsize::new(0));
        let callers: Vec<_> = (0..16)
            .map(|_| {
                let (state, runs, gate, joined) = (Arc::clone(&state), Arc::clone(&runs), Arc::clone(&gate), Arc::clone(&joined));

                tokio::spawn(async move {
                    let run = start(&state, &runs, &gate);

                    joined.fetch_add(1, Ordering::SeqCst);
                    run.await
                })
            })
            .collect();

        while joined.load(Ordering::SeqCst) < 16 {
            tokio::task::yield_now().await;
        }

        gate.add_permits(1);

        for caller in callers {
            assert_eq!(caller.await.unwrap(), 1);
        }

        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_run_ends_when_every_caller_stopped_waiting_and_the_next_call_starts_another() {
        let (state, runs, gate) = (Arc::default(), Arc::default(), Arc::new(tokio::sync::Semaphore::new(0)));

        drop(start(&state, &runs, &gate));
        gate.add_permits(1);

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&state).running.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await;

        assert!(ended.is_ok(), "the run did not end once its only caller stopped waiting");

        assert_eq!(start(&state, &runs, &gate).await, 2);
    }
}
