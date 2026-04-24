//! Clap CLI arguments for configuring beth-idle (implicit activation).

use std::time::Duration;

use clap::Args;

#[derive(Debug, Clone, Args)]
pub struct IdleCliArgs {
    /// Idle timer before triggering a graceful shutdown and suspending the VM.
    ///
    /// Examples: `5m`, `300s`, `1h`.
    ///
    /// Activation is implicit: if the flag is absent, beth-idle is disabled.
    #[arg(
        long = "beth-idle.timer",
        value_parser = humantime::parse_duration,
        num_args = 0..=1,
        default_missing_value = "5m"
    )]
    pub timer: Option<Duration>,
}

impl IdleCliArgs {
    pub fn to_config(&self) -> Result<beth_idle::IdleConfig, String> {
        let Some(timer) = self.timer else {
            return Err("missing required --beth-idle.timer".to_string());
        };

        beth_idle::IdleConfig::new(timer)
    }

    pub fn to_option_config(&self) -> Result<Option<beth_idle::IdleConfig>, String> {
        if self.timer.is_none() {
            return Ok(None);
        }

        Ok(Some(self.to_config()?))
    }
}

