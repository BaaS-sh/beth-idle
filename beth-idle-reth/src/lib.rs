pub use beth_idle::IdleConfig;

#[cfg(feature = "cli")]
mod runtime;

#[cfg(feature = "cli")]
pub use runtime::{run_idle_monitor, IdleEvent};
