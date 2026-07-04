//! Handler arity unification and route entry storage types.
//!
//! The `Into*Handler` traits erase the four supported handler shapes into one
//! boxed closure per route kind, and pair each with the [`RouteValidator`]
//! that makes typed captures part of route candidacy: a key that fails to
//! parse removes the route from dispatch instead of erroring the request.

use super::pattern::Pattern;
use crate::captures::{CaptureDescriptor, Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::Result;
use crate::handler::{DirCx, TreeRef};
use crate::projection::{DirListing, FileProjection};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed `'static` future the dispatch path awaits. Not `Send`: providers
/// are single-threaded WASM components.
type HandlerFuture<T> = Pin<Box<dyn Future<Output = Result<T>>>>;

/// The supported handler shapes box into one uniform call closure per route
/// kind; captures travel alongside the context so the closure can parse the
/// typed key at call time.
pub(super) type BoxedDirHandler<S> = Arc<dyn Fn(DirCx<S>, Captures) -> HandlerFuture<DirListing>>;
pub(super) type BoxedFileHandler<S> = Arc<dyn Fn(Cx<S>, Captures) -> HandlerFuture<FileProjection>>;
pub(super) type BoxedTreeRefHandler<S> = Arc<dyn Fn(Cx<S>, Captures) -> HandlerFuture<TreeRef>>;

/// A per-route capture validator derived from the handler's key type.
///
/// `full` runs `FromCaptures::from_captures` over a complete capture set and
/// gates route candidacy in dispatch: rejection means "this route does not
/// bind this path," and matching falls through to the next-most-specific
/// candidate. `present` runs
/// [`crate::captures::FromCaptures::validate_present_captures`] over a path
/// prefix where later captures are not yet available; static directory
/// discovery uses it so a future capture's absence cannot hide a literal
/// ancestor.
#[derive(Clone)]
pub struct RouteValidator {
    full: Arc<dyn Fn(&Captures) -> bool>,
    present: Arc<dyn Fn(&Captures) -> bool>,
    capture_descriptors: Vec<CaptureDescriptor>,
}

impl RouteValidator {
    pub(super) fn accepts(&self, caps: &Captures) -> bool {
        (self.full)(caps)
    }

    pub(super) fn accepts_present(&self, caps: &Captures) -> bool {
        (self.present)(caps)
    }

    pub(super) fn capture_descriptors(&self) -> &[CaptureDescriptor] {
        &self.capture_descriptors
    }
}

/// Accepted directory handler shapes. `Marker` exists only to keep the
/// blanket impls coherent (a closure could otherwise satisfy several); the
/// compiler infers it, authors never name it. Shapes: `async fn(DirCx<S>)`
/// ([`NoCaptures`]), `async fn(DirCx<S>, C)` ([`WithCaptures`]),
/// `async fn(C, DirCx<S>)` ([`WithKeyMethod`]), and sync `fn(C, DirCx<S>)`
/// ([`WithSyncKeyMethod`]), where `C: FromCaptures`.
pub trait IntoDirHandler<S, Marker> {
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator);
}

/// Accepted file handler shapes: `async fn(Cx<S>)`, `async fn(Cx<S>, C)`, or
/// `async fn(C, Cx<S>)`, where `C: FromCaptures`. See [`IntoDirHandler`] for
/// the role of `Marker`.
pub trait IntoFileHandler<S, Marker> {
    fn into_file_handler(self) -> (BoxedFileHandler<S>, RouteValidator);
}

/// Accepted treeref handler shapes: `async fn(Cx<S>)`, `async fn(Cx<S>, C)`,
/// or `async fn(C, Cx<S>)`, returning [`TreeRef`]. See [`IntoDirHandler`]
/// for the role of `Marker`.
pub trait IntoTreeRefHandler<S, Marker> {
    fn into_treeref_handler(self) -> (BoxedTreeRefHandler<S>, RouteValidator);
}

/// Marker: context-only handlers, `fn(Cx)` / `fn(DirCx)`.
#[doc(hidden)]
pub struct NoCaptures(());
/// Marker: context-first captured handlers, `fn(Cx, Key)` / `fn(DirCx, Key)`.
#[doc(hidden)]
pub struct WithCaptures<C>(core::marker::PhantomData<C>);
/// Marker: key-first captured handlers, `fn(Key, Cx)` / `fn(Key, DirCx)`.
#[doc(hidden)]
pub struct WithKeyMethod<C>(core::marker::PhantomData<C>);
/// Marker: key-first synchronous dir handlers, `fn(Key, DirCx)`.
#[doc(hidden)]
pub struct WithSyncKeyMethod<C>(core::marker::PhantomData<C>);

/// The validator pair for a typed key `C`; this is the bridge that turns a
/// `FromStr` rejection in a `#[path_captures]` field into route fallthrough.
pub(super) fn captures_validator<C: FromCaptures>() -> RouteValidator {
    RouteValidator {
        full: Arc::new(|caps: &Captures| C::from_captures(caps).is_ok()),
        present: Arc::new(|caps: &Captures| C::validate_present_captures(caps)),
        capture_descriptors: C::capture_descriptors(),
    }
}

/// The validator for capture-less handlers: every path the pattern matches
/// is accepted.
pub(super) fn accept_validator() -> RouteValidator {
    RouteValidator {
        full: Arc::new(|_caps: &Captures| true),
        present: Arc::new(|_caps: &Captures| true),
        capture_descriptors: Vec::new(),
    }
}

impl<S, F, Fut> IntoDirHandler<S, NoCaptures> for F
where
    F: Fn(DirCx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<DirListing>> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        (
            Arc::new(move |cx: DirCx<S>, _caps: Captures| Box::pin(self(cx))),
            accept_validator(),
        )
    }
}

impl<S, C, F, Fut> IntoDirHandler<S, WithCaptures<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(DirCx<S>, C) -> Fut + 'static,
    Fut: Future<Output = Result<DirListing>> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(cx, parsed)) as HandlerFuture<DirListing>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, C, F, Fut> IntoDirHandler<S, WithKeyMethod<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, DirCx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<DirListing>> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(parsed, cx)) as HandlerFuture<DirListing>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, C, F> IntoDirHandler<S, WithSyncKeyMethod<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, DirCx<S>) -> Result<DirListing> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => {
                        let result = self(parsed, cx);
                        Box::pin(async move { result }) as HandlerFuture<DirListing>
                    },
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, F, Fut> IntoFileHandler<S, NoCaptures> for F
where
    F: Fn(Cx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<FileProjection>> + 'static,
{
    fn into_file_handler(self) -> (BoxedFileHandler<S>, RouteValidator) {
        (
            Arc::new(move |cx: Cx<S>, _caps: Captures| Box::pin(self(cx))),
            accept_validator(),
        )
    }
}

impl<S, C, F, Fut> IntoFileHandler<S, WithCaptures<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(Cx<S>, C) -> Fut + 'static,
    Fut: Future<Output = Result<FileProjection>> + 'static,
{
    fn into_file_handler(self) -> (BoxedFileHandler<S>, RouteValidator) {
        let handler: BoxedFileHandler<S> =
            Arc::new(
                move |cx: Cx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(cx, parsed)) as HandlerFuture<FileProjection>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, C, F, Fut> IntoFileHandler<S, WithKeyMethod<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, Cx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<FileProjection>> + 'static,
{
    fn into_file_handler(self) -> (BoxedFileHandler<S>, RouteValidator) {
        let handler: BoxedFileHandler<S> =
            Arc::new(
                move |cx: Cx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(parsed, cx)) as HandlerFuture<FileProjection>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, F, Fut> IntoTreeRefHandler<S, NoCaptures> for F
where
    F: Fn(Cx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<TreeRef>> + 'static,
{
    fn into_treeref_handler(self) -> (BoxedTreeRefHandler<S>, RouteValidator) {
        (
            Arc::new(move |cx: Cx<S>, _caps: Captures| Box::pin(self(cx))),
            accept_validator(),
        )
    }
}

impl<S, C, F, Fut> IntoTreeRefHandler<S, WithCaptures<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(Cx<S>, C) -> Fut + 'static,
    Fut: Future<Output = Result<TreeRef>> + 'static,
{
    fn into_treeref_handler(self) -> (BoxedTreeRefHandler<S>, RouteValidator) {
        let handler: BoxedTreeRefHandler<S> =
            Arc::new(
                move |cx: Cx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(cx, parsed)) as HandlerFuture<TreeRef>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, C, F, Fut> IntoTreeRefHandler<S, WithKeyMethod<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, Cx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<TreeRef>> + 'static,
{
    fn into_treeref_handler(self) -> (BoxedTreeRefHandler<S>, RouteValidator) {
        let handler: BoxedTreeRefHandler<S> =
            Arc::new(
                move |cx: Cx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(parsed, cx)) as HandlerFuture<TreeRef>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

/// One row of the dir route table: pattern, erased handler, validator.
pub(super) struct DirEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedDirHandler<S>,
    pub(super) validator: RouteValidator,
}

pub(super) struct FileEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedFileHandler<S>,
    pub(super) validator: RouteValidator,
    /// The route was declared `ranged`, so its listing/lookup placeholder
    /// projects `ReadMode::Ranged` and the host dispatches `open` straight to
    /// `open-file` without probing. The handler still supplies the real reader,
    /// size, and stability at `open-file`.
    pub(super) ranged: bool,
}

pub(super) struct TreeRefEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedTreeRefHandler<S>,
    pub(super) validator: RouteValidator,
}
