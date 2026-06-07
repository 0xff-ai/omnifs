//! Handler arity unification and route entry storage types.

use crate::captures::{Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::Result;
use crate::handler::{DirCx, TreeRef};
use crate::projection::{DirProjection, FileProjection};
use omnifs_core::path::Pattern;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed `'static` future the dispatch path awaits.
type HandlerFuture<T> = Pin<Box<dyn Future<Output = Result<T>>>>;

/// The supported `dir` handler shapes box into a uniform call closure.
pub(super) type BoxedDirHandler<S> =
    Arc<dyn Fn(DirCx<S>, Captures) -> HandlerFuture<DirProjection>>;
pub(super) type BoxedFileHandler<S> = Arc<dyn Fn(Cx<S>, Captures) -> HandlerFuture<FileProjection>>;
pub(super) type BoxedTreeRefHandler<S> = Arc<dyn Fn(Cx<S>, Captures) -> HandlerFuture<TreeRef>>;

/// A per-route capture validator.
#[derive(Clone)]
pub struct RouteValidator {
    full: Arc<dyn Fn(&Captures) -> bool>,
    present: Arc<dyn Fn(&Captures) -> bool>,
}

impl RouteValidator {
    pub(super) fn accepts(&self, caps: &Captures) -> bool {
        (self.full)(caps)
    }

    pub(super) fn accepts_present(&self, caps: &Captures) -> bool {
        (self.present)(caps)
    }
}

pub trait IntoDirHandler<S, Marker> {
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator);
}

pub trait IntoFileHandler<S, Marker> {
    fn into_file_handler(self) -> (BoxedFileHandler<S>, RouteValidator);
}

pub trait IntoTreeRefHandler<S, Marker> {
    fn into_treeref_handler(self) -> (BoxedTreeRefHandler<S>, RouteValidator);
}

#[doc(hidden)]
pub struct NoCaptures(());
#[doc(hidden)]
pub struct WithCaptures<C>(core::marker::PhantomData<C>);
/// Captured route handlers keyed as `fn(Key, Cx)` / `fn(Key, DirCx)`.
#[doc(hidden)]
pub struct WithKeyMethod<C>(core::marker::PhantomData<C>);
/// Captured route handlers keyed as synchronous `fn(Key, DirCx)`.
#[doc(hidden)]
pub struct WithSyncKeyMethod<C>(core::marker::PhantomData<C>);

pub(super) fn captures_validator<C: FromCaptures>() -> RouteValidator {
    RouteValidator {
        full: Arc::new(|caps: &Captures| C::from_captures(caps).is_ok()),
        present: Arc::new(|caps: &Captures| C::validate_present_captures(caps)),
    }
}

pub(super) fn accept_validator() -> RouteValidator {
    RouteValidator {
        full: Arc::new(|_caps: &Captures| true),
        present: Arc::new(|_caps: &Captures| true),
    }
}

impl<S, F, Fut> IntoDirHandler<S, NoCaptures> for F
where
    F: Fn(DirCx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<DirProjection>> + 'static,
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
    Fut: Future<Output = Result<DirProjection>> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(cx, parsed)) as HandlerFuture<DirProjection>,
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
    Fut: Future<Output = Result<DirProjection>> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => Box::pin(self(parsed, cx)) as HandlerFuture<DirProjection>,
                    Err(error) => Box::pin(async move { Err(error) }),
                },
            );
        (handler, captures_validator::<C>())
    }
}

impl<S, C, F> IntoDirHandler<S, WithSyncKeyMethod<C>> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, DirCx<S>) -> Result<DirProjection> + 'static,
{
    fn into_dir_handler(self) -> (BoxedDirHandler<S>, RouteValidator) {
        let handler: BoxedDirHandler<S> =
            Arc::new(
                move |cx: DirCx<S>, caps: Captures| match C::from_captures(&caps) {
                    Ok(parsed) => {
                        let result = self(parsed, cx);
                        Box::pin(async move { result }) as HandlerFuture<DirProjection>
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

pub(super) struct DirEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedDirHandler<S>,
    pub(super) validator: RouteValidator,
}

pub(super) struct FileEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedFileHandler<S>,
    pub(super) validator: RouteValidator,
}

pub(super) struct TreeRefEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: BoxedTreeRefHandler<S>,
    pub(super) validator: RouteValidator,
}

/// A typed route handle returned at registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteHandle(pub u32);
