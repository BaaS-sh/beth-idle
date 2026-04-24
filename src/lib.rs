use std::time::Duration;

/// Default idle timer (5 minutes).
pub const DEFAULT_IDLE_TIMER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleConfig {
    pub idle_timer: Duration,
}

impl IdleConfig {
    pub fn new(idle_timer: Duration) -> Result<Self, String> {
        let cfg = Self { idle_timer };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.idle_timer.is_zero() {
            return Err("beth-idle.timer must be > 0".to_string());
        }
        Ok(())
    }
}

impl Default for IdleConfig {
    fn default() -> Self {
        Self {
            idle_timer: DEFAULT_IDLE_TIMER,
        }
    }
}

#[cfg(feature = "gce-sdk")]
pub mod gce;

#[cfg(feature = "gce-sdk")]
mod retry;

