//! `lookup_child`, `list_children`, `read_file`, and `open_file` dispatch.
//!
//! Each entry point resolves an absolute path against the compiled route
//! tables through a shared `Shape` view (`route_shape`), with the
//! literal-prefix auto-navigation machinery in `static_shape`. Common rules
//! across all entry points:
//!
//! - Candidate selection is per route kind, highest precedence first, with
//!   capture validators filtering candidacy (a typed-key parse rejection
//!   falls through to the next-most-specific route, not to not-found).
//! - Treeref routes win before anything else; below a handed-off subtree the
//!   host never calls the provider again.
//! - Literal prefixes of registered routes resolve and list as directories
//!   with no handler involved; listings merge handler enumerations with
//!   those static siblings and are non-exhaustive whenever a capture
//!   sibling exists at the next depth.

mod list;
mod lookup;
mod read;
mod route_shape;
mod static_shape;

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::{ProviderError, Result};

pub(super) fn route_future<'a, T>(
    template: String,
    future: Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
) -> RouteFuture<'a, T> {
    RouteFuture { template, future }
}

pub(super) struct RouteFuture<'a, T> {
    template: String,
    future: Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
}

impl<T> Future for RouteFuture<'_, T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.future.as_mut().poll(cx))) {
            Ok(result) => result,
            Err(payload) => Poll::Ready(Err(ProviderError::internal(format!(
                "provider handler panicked [route_template={}; panic={}]",
                self.template,
                panic_payload_message(payload.as_ref())
            )))),
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}
