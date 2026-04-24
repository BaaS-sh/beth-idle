use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Default)]
pub struct IdleState {
    triggered: AtomicBool,
    event: Mutex<Option<IdleEvent>>,
}

impl IdleState {
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_triggered(&self) -> bool {
        self.triggered.load(Ordering::Acquire)
    }

    fn record_event(&self, event: IdleEvent) -> bool {
        let mut guard = self.event.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return false;
        }
        *guard = Some(event);
        self.triggered.store(true, Ordering::Release);
        true
    }

    fn take_event_for_suspend(&self) -> Option<IdleEvent> {
        let mut guard = self.event.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    }
}

#[derive(Debug, Clone)]
struct IdleSchedulerState {
    idle_window: Duration,
    last_best_block: Option<u64>,
    last_seen_at: Option<tokio::time::Instant>,
    next_idle_at: Option<tokio::time::Instant>,
    triggered: bool,
}

impl IdleSchedulerState {
    fn new(idle_window: Duration) -> Self {
        Self {
            idle_window,
            last_best_block: None,
            last_seen_at: None,
            next_idle_at: None,
            triggered: false,
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
        !self.triggered
            && self.last_best_block.is_some()
            && self
                .next_idle_at
                .map(|t| now >= t)
                .unwrap_or_default()
    }

    fn trigger(&mut self) {
        self.triggered = true;
    }
}

pub async fn run_idle_monitor<F>(
    best_block_number: F,
    cfg: beth_idle::IdleConfig,
    shutdown: CancellationToken,
    state: Arc<IdleState>,
) where
    F: Fn() -> eyre::Result<u64> + Send + Sync + 'static,
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
                if state.is_triggered() {
                    break;
                }

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

                if !state.record_event(event.clone()) {
                    break;
                }

                tracing::info!(
                    target: "reth::cli",
                    last_best_block_number = event.last_best_block_number,
                    idle_timer_secs = event.idle_timer.as_secs(),
                    last_seen_elapsed_secs = event.last_seen_elapsed_secs,
                    triggered_elapsed_secs = event.triggered_elapsed_secs,
                    request_id = %event.request_id,
                    "beth-idle: idle window reached; requesting graceful shutdown"
                );

                shutdown.cancel();
                break;
            }
        }
    }

    tracing::info!(target: "reth::cli", "beth-idle monitor stopped");
}

pub async fn suspend_gce_if_triggered(state: Arc<IdleState>) {
    let Some(event) = state.take_event_for_suspend() else {
        return;
    };

    tracing::info!(
        target: "reth::cli",
        last_best_block_number = event.last_best_block_number,
        idle_timer_secs = event.idle_timer.as_secs(),
        request_id = %event.request_id,
        "beth-idle: idle shutdown confirmed; suspending GCE instance"
    );

    if let Err(err) = beth_idle::gce::suspend_self(false, event.request_id)
        .await
        .wrap_err("beth-idle: suspend_self failed")
    {
        tracing::error!(
            target: "reth::cli",
            %err,
            request_id = %event.request_id,
            "beth-idle: GCE suspend failed"
        );
    } else {
        tracing::info!(
            target: "reth::cli",
            request_id = %event.request_id,
            "beth-idle: GCE suspend succeeded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{IdleEvent, IdleSchedulerState, IdleState};
    use std::time::Duration;

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
    fn trigger_is_idempotent() {
        let idle = Duration::from_secs(300);
        let start = tokio::time::Instant::now();
        let mut s = IdleSchedulerState::new(idle);

        assert!(s.observe_best_block(1, start));
        assert!(s.due(start + idle));
        s.trigger();
        assert!(!s.due(start + idle + Duration::from_secs(1)));
    }

    #[test]
    fn gcp_call_is_consumed_once() {
        let state = IdleState::new_shared();
        let event = IdleEvent {
            request_id: uuid::Uuid::new_v4(),
            last_best_block_number: 123,
            idle_timer: Duration::from_secs(300),
            last_seen_elapsed_secs: 10,
            triggered_elapsed_secs: 310,
        };

        assert!(state.record_event(event));
        assert!(state.take_event_for_suspend().is_some());
        assert!(state.take_event_for_suspend().is_none());
    }
}
