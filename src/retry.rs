use std::time::Duration;

pub(crate) enum RetryError {
    Retryable(eyre::Report),
    Permanent(eyre::Report),
}

impl RetryError {
    pub(crate) fn retryable(e: eyre::Report) -> Self {
        Self::Retryable(e)
    }

    pub(crate) fn permanent(e: eyre::Report) -> Self {
        Self::Permanent(e)
    }
}

pub(crate) async fn retry_with_backoff<T, F, Fut>(
    mut f: F,
    max_retries: usize,
    attempt_timeout: Duration,
    backoff_base: Duration,
) -> eyre::Result<T>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, RetryError>>,
{
    let mut tries: usize = 0;
    loop {
        let attempt = tries + 1;
        let res = tokio::time::timeout(attempt_timeout, f(attempt)).await;

        let retryable_err = match res {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(RetryError::Permanent(e))) => return Err(e),
            Ok(Err(RetryError::Retryable(e))) => e,
            Err(_) => eyre::eyre!("beth-idle: attempt {attempt} timed out after {attempt_timeout:?}"),
        };

        if tries >= max_retries {
            return Err(retryable_err);
        }

        let backoff = backoff_base.saturating_mul((tries + 1) as u32);
        tracing::warn!(
            target: "reth::cli",
            attempt,
            max_retries,
            backoff_ms = backoff.as_millis() as u64,
            err = %retryable_err,
            "beth-idle: retrying after failure"
        );
        tokio::time::sleep(backoff).await;
        tries += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{retry_with_backoff, RetryError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn retries_are_bounded_and_eventually_fail() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_f = calls.clone();

        let res: eyre::Result<()> = retry_with_backoff(
            move |_attempt| {
                let calls = calls_for_f.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(RetryError::retryable(eyre::eyre!("boom")))
                }
            },
            2,                     // max_retries
            Duration::from_secs(1), // attempt_timeout
            Duration::from_millis(0),
        )
        .await;

        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
