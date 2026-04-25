use std::future::Future;
use std::time::Duration;

use eyre::WrapErr as _;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct IdleEvent {
    pub request_id: uuid::Uuid,
    pub last_best_block_number: u64,
    pub idle_timer: Duration,
    pub last_seen_elapsed_secs: u64,
    pub triggered_elapsed_secs: u64,
}

#[derive(Debug, Clone)]
struct IdleSchedulerState {
    idle_window: Duration,
    last_best_block: Option<u64>,
    last_seen_at: Option<tokio::time::Instant>,
    next_idle_at: Option<tokio::time::Instant>,
    suspending: bool,
}

impl IdleSchedulerState {
    fn new(idle_window: Duration) -> Self {
        Self {
            idle_window,
            last_best_block: None,
            last_seen_at: None,
            next_idle_at: None,
            suspending: false,
        }
    }

    fn observe_best_block(&mut self, best: u64, now: tokio::time::Instant) -> bool {
        match self.last_best_block {
            None => {
                self.last_best_block = Some(best);
                self.last_seen_at = Some(now);
                self.next_idle_at = Some(now + self.idle_window);
                true
            }
            Some(prev) if prev != best => {
                self.last_best_block = Some(best);
                self.last_seen_at = Some(now);
                self.next_idle_at = Some(now + self.idle_window);
                true
            }
            Some(_) => false,
        }
    }

    fn postpone(&mut self, now: tokio::time::Instant) {
        if self.last_best_block.is_some() {
            self.next_idle_at = Some(now + self.idle_window);
        }
    }

    fn due(&self, now: tokio::time::Instant) -> bool {
        !self.suspending
            && self.last_best_block.is_some()
            && self.next_idle_at.map(|t| now >= t).unwrap_or_default()
    }

    fn trigger(&mut self) {
        self.suspending = true;
    }

    fn reset_after_suspend_attempt(&mut self, now: tokio::time::Instant) {
        self.suspending = false;
        if self.last_best_block.is_some() {
            self.last_seen_at = Some(now);
            self.next_idle_at = Some(now + self.idle_window);
        }
    }
}

pub async fn run_idle_monitor<F>(
    best_block_number: F,
    cfg: beth_idle::IdleConfig,
    shutdown: CancellationToken,
) where
    F: Fn() -> eyre::Result<u64> + Send + Sync + 'static,
{
    run_idle_monitor_with_suspend(best_block_number, cfg, shutdown, |event| async move {
        beth_idle::gce::suspend_self(false, event.request_id)
            .await
            .wrap_err("beth-idle: suspend_self failed")
    })
    .await;
}

async fn run_idle_monitor_with_suspend<F, S, Fut>(
    best_block_number: F,
    cfg: beth_idle::IdleConfig,
    shutdown: CancellationToken,
    mut suspend: S,
) where
    F: Fn() -> eyre::Result<u64> + Send + Sync + 'static,
    S: FnMut(IdleEvent) -> Fut,
    Fut: Future<Output = eyre::Result<()>>,
{
    let idle_window = cfg.idle_timer;
    let poll_interval = Duration::from_secs(1);

    let started_at = tokio::time::Instant::now();
    let mut scheduler = IdleSchedulerState::new(idle_window);
    let mut tick = tokio::time::interval(poll_interval);

    tracing::info!(
        target: "reth::cli",
        idle_timer_secs = idle_window.as_secs(),
        poll_interval_secs = poll_interval.as_secs(),
        "beth-idle monitor started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();

                match best_block_number() {
                    Ok(best) => {
                        if scheduler.observe_best_block(best, now) {
                            let elapsed = now.duration_since(started_at).as_secs();
                            tracing::info!(
                                target: "reth::cli",
                                best_block_number = best,
                                elapsed_secs = elapsed,
                                idle_timer_secs = idle_window.as_secs(),
                                "beth-idle: block observed; idle timer (re)started"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "reth::cli",
                            %err,
                            "beth-idle: failed to poll best block number; postponing idle deadline"
                        );
                        scheduler.postpone(now);
                    }
                }

                if !scheduler.due(now) {
                    continue;
                }

                scheduler.trigger();
                let last_best = scheduler.last_best_block.unwrap_or_default();
                let last_seen_at = scheduler.last_seen_at.unwrap_or(started_at);

                let event = IdleEvent {
                    request_id: uuid::Uuid::new_v4(),
                    last_best_block_number: last_best,
                    idle_timer: idle_window,
                    last_seen_elapsed_secs: last_seen_at.duration_since(started_at).as_secs(),
                    triggered_elapsed_secs: now.duration_since(started_at).as_secs(),
                };

                tracing::info!(
                    target: "reth::cli",
                    last_best_block_number = event.last_best_block_number,
                    idle_timer_secs = event.idle_timer.as_secs(),
                    last_seen_elapsed_secs = event.last_seen_elapsed_secs,
                    triggered_elapsed_secs = event.triggered_elapsed_secs,
                    request_id = %event.request_id,
                    "beth-idle: idle window reached; suspending GCE instance"
                );

                let suspend_result = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    res = suspend(event.clone()) => res,
                };

                match suspend_result {
                    Ok(()) => {
                        tracing::info!(
                            target: "reth::cli",
                            request_id = %event.request_id,
                            "beth-idle: GCE suspend completed; idle monitor resumed"
                        );
                    }
                    Err(err) => {
                        tracing::error!(
                            target: "reth::cli",
                            %err,
                            request_id = %event.request_id,
                            "beth-idle: GCE suspend failed; retrying after next idle window"
                        );
                    }
                }

                scheduler.reset_after_suspend_attempt(tokio::time::Instant::now());
            }
        }
    }

    tracing::info!(target: "reth::cli", "beth-idle monitor stopped");
}

#[cfg(test)]
mod tests {
    use super::{run_idle_monitor_with_suspend, IdleSchedulerState};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn reset_timer_on_new_block() {
        let idle = Duration::from_secs(300);
        let start = tokio::time::Instant::now();
        let mut s = IdleSchedulerState::new(idle);

        assert!(s.observe_best_block(10, start));
        let new_block_at = start + Duration::from_secs(60);
        assert!(s.observe_best_block(11, new_block_at));

        assert!(!s.due(new_block_at + Duration::from_secs(299)));
        assert!(s.due(new_block_at + idle));
    }

    #[test]
    fn triggers_after_idle_window() {
        let idle = Duration::from_secs(300);
        let start = tokio::time::Instant::now();
        let mut s = IdleSchedulerState::new(idle);

        assert!(s.observe_best_block(42, start));
        assert!(!s.due(start + Duration::from_secs(299)));
        assert!(s.due(start + idle));
    }

    #[test]
    fn trigger_is_reset_after_suspend_attempt() {
        let idle = Duration::from_secs(300);
        let start = tokio::time::Instant::now();
        let mut s = IdleSchedulerState::new(idle);

        assert!(s.observe_best_block(1, start));
        assert!(s.due(start + idle));
        s.trigger();
        assert!(!s.due(start + idle + Duration::from_secs(1)));

        let resumed_at = start + idle + Duration::from_secs(10);
        s.reset_after_suspend_attempt(resumed_at);
        assert!(!s.due(resumed_at + Duration::from_secs(299)));
        assert!(s.due(resumed_at + idle));
    }

    #[tokio::test(start_paused = true)]
    async fn monitor_suspends_without_cancelling_shutdown_and_cycles() {
        let best_block = Arc::new(AtomicU64::new(1));
        let suspend_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();

        let monitor_best = best_block.clone();
        let monitor_shutdown = shutdown.clone();
        let monitor_calls = suspend_calls.clone();
        let handle = tokio::spawn(async move {
            run_idle_monitor_with_suspend(
                move || Ok(monitor_best.load(Ordering::SeqCst)),
                beth_idle::IdleConfig::new(Duration::from_secs(2)).unwrap(),
                monitor_shutdown,
                move |_event| {
                    let calls = monitor_calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                },
            )
            .await;
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        assert_eq!(suspend_calls.load(Ordering::SeqCst), 1);
        assert!(!shutdown.is_cancelled());

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        assert_eq!(suspend_calls.load(Ordering::SeqCst), 2);
        assert!(!shutdown.is_cancelled());

        shutdown.cancel();
        handle.await.unwrap();
    }
}
