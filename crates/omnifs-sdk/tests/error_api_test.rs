use omnifs_sdk::error::{ProviderError, ProviderErrorKind};
use omnifs_sdk::omnifs::provider::types::{OpResult, ProviderReturn};

#[test]
fn provider_error_maps_http_statuses_to_typed_kinds() {
    assert_eq!(
        ProviderError::from_http_status(401).kind(),
        ProviderErrorKind::PermissionDenied
    );
    assert_eq!(
        ProviderError::from_http_status(403).kind(),
        ProviderErrorKind::Denied
    );
    assert_eq!(
        ProviderError::from_http_status(404).kind(),
        ProviderErrorKind::NotFound
    );
    let rate_limited = ProviderError::from_http_status(429);
    assert_eq!(rate_limited.kind(), ProviderErrorKind::RateLimited);
    assert!(rate_limited.is_retryable());
}

#[test]
fn provider_error_into_response_preserves_retryable_flag() {
    let response: ProviderReturn = ProviderError::denied("final denial").into();

    let ProviderReturn {
        result: OpResult::Error(error),
        ..
    } = response
    else {
        panic!("expected provider error return");
    };
    assert_eq!(
        error.kind,
        omnifs_sdk::omnifs::provider::types::ErrorKind::Denied
    );
    assert!(!error.retryable);
}
