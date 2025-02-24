use crate::llm::{ApiError, ApiErrorContext, RateLimitHandler};
use anyhow::Result;
use reqwest::{Response, StatusCode};
use std::time::Duration;
use tokio::time::sleep;
use tracing::warn;

/// Check response error and extract rate limit information.
/// Returns Ok(Response) if successful, or an error with rate limit context if not.
pub async fn check_response_error<T: RateLimitHandler + std::fmt::Debug + Send + Sync + 'static>(response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let rate_limits = T::from_response(&response);
    let response_text = response
        .text()
        .await
        .map_err(|e| ApiError::NetworkError(e.to_string()))?;

    let error = match status {
        StatusCode::TOO_MANY_REQUESTS => ApiError::RateLimit(response_text),
        StatusCode::UNAUTHORIZED => ApiError::Authentication(response_text),
        StatusCode::BAD_REQUEST => ApiError::InvalidRequest(response_text),
        status if status.is_server_error() => ApiError::ServiceError(response_text),
        _ => ApiError::Unknown(format!("Status {}: {}", status, response_text)),
    };

    Err(ApiErrorContext {
        error,
        rate_limits: Some(rate_limits),
    }
    .into())
}

/// Handle retryable errors and rate limiting for LLM providers.
/// Returns true if the error is retryable and we should continue the retry loop.
/// Returns false if we should exit the retry loop.
pub async fn handle_retryable_error<
    T: RateLimitHandler + std::fmt::Debug + Send + Sync + 'static,
>(
    error: &anyhow::Error,
    attempts: u32,
    max_retries: u32,
) -> bool {
    if let Some(ctx) = error.downcast_ref::<ApiErrorContext<T>>() {
        match &ctx.error {
            ApiError::RateLimit(_) => {
                if let Some(rate_limits) = &ctx.rate_limits {
                    if attempts < max_retries {
                        let delay = rate_limits.get_retry_delay();
                        warn!(
                            "Rate limit hit (attempt {}/{}), waiting {} seconds before retry",
                            attempts,
                            max_retries,
                            delay.as_secs()
                        );
                        sleep(delay).await;
                        return true;
                    }
                } else {
                    // Fallback if no rate limit info available
                    if attempts < max_retries {
                        let delay = Duration::from_secs(2u64.pow(attempts - 1));
                        warn!(
                            "Rate limit hit but no timing info available (attempt {}/{}), using exponential backoff: {} seconds",
                            attempts,
                            max_retries,
                            delay.as_secs()
                        );
                        sleep(delay).await;
                        return true;
                    }
                }
            }
            ApiError::ServiceError(_) | ApiError::NetworkError(_) => {
                if attempts < max_retries {
                    let delay = Duration::from_secs(2u64.pow(attempts - 1));
                    warn!(
                        "Error: {} (attempt {}/{}), retrying in {} seconds",
                        error,
                        attempts,
                        max_retries,
                        delay.as_secs()
                    );
                    sleep(delay).await;
                    return true;
                }
            }
            _ => {
                warn!(
                    "Unhandled error (attempt {}/{}): {:?}",
                    attempts, max_retries, error
                );
            }
        }
    }
    false
}
