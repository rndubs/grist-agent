//! Provider call with retry and backoff (§7.2, D15).

use std::time::Duration;

use super::Shared;
use crate::cancel::CancellationToken;
use crate::config::RetryPolicy;
use crate::event::{EventBody, ProviderRetryPayload};
use crate::log::LogError;
use crate::provider::{DeltaStream, ModelDelta, ModelRequest, ModelResponse, ProviderError};

/// Why `call_with_retry` gave up.
pub(super) enum CallFailure {
    /// The turn token fired; the in-flight attempt was dropped.
    Cancelled,
    /// Retries exhausted or a non-retryable error.
    Failed { error: ProviderError, attempts: u32 },
    /// `provider_retry` could not be written.
    Log(LogError),
}

/// A fresh `request_id` (UUIDv7) per attempt.
pub(super) fn new_request_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// `delay = min(max_delay, base × multiplier^(attempt−1))`, full jitter when enabled,
/// `RateLimited{retry_after}` as a lower bound (capped at `max_delay`).
pub(super) fn backoff_delay(p: &RetryPolicy, attempt: u32, err: &ProviderError) -> Duration {
    let exp = p.multiplier.powi(attempt.saturating_sub(1) as i32);
    let raw = p.base_delay.as_secs_f64() * exp;
    let capped = if raw.is_finite() {
        raw.clamp(0.0, p.max_delay.as_secs_f64())
    } else {
        p.max_delay.as_secs_f64()
    };
    let mut delay = if p.jitter {
        Duration::from_secs_f64(rand::random::<f64>() * capped)
    } else {
        Duration::from_secs_f64(capped)
    };
    if let ProviderError::RateLimited {
        retry_after: Some(ra),
    } = err
    {
        delay = delay.max((*ra).min(p.max_delay));
    }
    delay
}

async fn next_delta(s: &mut DeltaStream) -> Option<Result<ModelDelta, ProviderError>> {
    std::future::poll_fn(|cx| s.as_mut().poll_next(cx)).await
}

/// One attempt: `complete_stream` (forwarding deltas to the sink, taking the `Complete`) when a
/// delta sink is configured, else `complete`.
async fn one_attempt(sh: &Shared, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
    match &sh.delta_sink {
        None => sh.provider.complete(req).await,
        Some(sink) => {
            let mut stream = sh.provider.complete_stream(req).await?;
            loop {
                match next_delta(&mut stream).await {
                    Some(Ok(ModelDelta::Complete(resp))) => {
                        let _ = sink.send(ModelDelta::Complete(resp.clone()));
                        return Ok(resp);
                    }
                    Some(Ok(delta)) => {
                        let _ = sink.send(delta);
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        return Err(ProviderError::InvalidResponse(
                            "stream ended without a Complete item".to_owned(),
                        ));
                    }
                }
            }
        }
    }
}

/// C1 of §6. Every attempt shares `request_hash`; `trace.attempt` and `trace.request_id` vary.
pub(super) async fn call_with_retry(
    sh: &Shared,
    turn: u64,
    mut req: ModelRequest,
    tt: &CancellationToken,
) -> Result<(ModelResponse, u32), CallFailure> {
    let policy = &sh.retry;
    let mut attempt: u32 = 1;
    loop {
        req.trace.attempt = attempt;
        if attempt > 1 {
            req.trace.request_id = new_request_id();
        }
        let res = tokio::select! {
            biased;
            _ = tt.cancelled() => return Err(CallFailure::Cancelled),
            r = tokio::time::timeout(policy.request_timeout, one_attempt(sh, req.clone())) => {
                r.unwrap_or(Err(ProviderError::Timeout(policy.request_timeout)))
            }
        };
        match res {
            Ok(resp) => return Ok((resp, attempt)),
            Err(error) => {
                if tt.is_cancelled() {
                    return Err(CallFailure::Cancelled);
                }
                if error.retryable() && attempt < policy.max_attempts {
                    let delay = backoff_delay(policy, attempt, &error);
                    let (message, _) = sh.redactor.redact_str(&error.to_string());
                    sh.logger
                        .log(EventBody::ProviderRetry(ProviderRetryPayload {
                            turn,
                            attempt,
                            error_class: error.class().to_owned(),
                            message,
                            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        }))
                        .await
                        .map_err(CallFailure::Log)?;
                    tokio::select! {
                        biased;
                        _ = tt.cancelled() => return Err(CallFailure::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    attempt += 1;
                } else {
                    return Err(CallFailure::Failed {
                        error,
                        attempts: attempt,
                    });
                }
            }
        }
    }
}
