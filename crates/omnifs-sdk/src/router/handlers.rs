//! Handler inference, the normalized route ABI, and route entry storage.

use super::pattern::Pattern;
use crate::captures::{CaptureDescriptor, Captures, FromCaptures};
use crate::cx::Cx;
use crate::error::Result;
use crate::handler::{DirCx, TreeRef};
use crate::projection::{DirListing, FileProjection};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed future returned by the normalized handler ABI.
pub(super) type HandlerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + 'a>>;

type HandlerCall<C, I, O> = dyn for<'a> Fn(&'a C, I, Captures) -> HandlerFuture<'a, O>;

/// The one internal route handler ABI. `C` is the operation context, `I` is
/// typed operation input beyond captures, and `O` is the operation-specific
/// result. The validator remains part of the carrier so route candidacy and
/// execution cannot drift apart.
#[doc(hidden)]
pub struct Handler<C, I, O> {
    call: Arc<HandlerCall<C, I, O>>,
    validator: RouteValidator,
}

impl<C, I, O> Clone for Handler<C, I, O> {
    fn clone(&self) -> Self {
        Self {
            call: self.call.clone(),
            validator: self.validator.clone(),
        }
    }
}

impl<C, I, O> Handler<C, I, O> {
    pub(super) fn new(call: Arc<HandlerCall<C, I, O>>, validator: RouteValidator) -> Self {
        Self { call, validator }
    }

    pub(super) fn call<'a>(
        &'a self,
        context: &'a C,
        input: I,
        captures: Captures,
    ) -> HandlerFuture<'a, O> {
        (self.call)(context, input, captures)
    }

    pub(super) fn validator(&self) -> &RouteValidator {
        &self.validator
    }

    pub(super) fn bind_input(self, input: I) -> Handler<C, (), O>
    where
        C: 'static,
        I: Clone + 'static,
        O: 'static,
    {
        let validator = self.validator.clone();
        Handler::new(
            Arc::new(move |context, (), captures| {
                let handler = self.clone();
                let input = input.clone();
                Box::pin(async move { handler.call(context, input, captures).await })
            }),
            validator,
        )
    }
}

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

/// Hidden inference surface for the supported author function tuples. The
/// associated types normalize every tuple into one operation context and one
/// typed extra-input slot; current author handlers use `Input = ()`.
#[doc(hidden)]
pub trait IntoHandler<S, Args, Output> {
    type Context;
    type Input;

    fn into_handler(self) -> Handler<Self::Context, Self::Input, Output>;
}

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

impl<S, F, Fut, O> IntoHandler<S, (DirCx<S>,), O> for F
where
    F: Fn(DirCx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<O>> + 'static,
{
    type Context = DirCx<S>;
    type Input = ();

    fn into_handler(self) -> Handler<Self::Context, Self::Input, O> {
        Handler::new(
            Arc::new(move |context: &DirCx<S>, (), _captures: Captures| {
                let context = DirCx::new((**context).clone(), context.intent().clone());
                Box::pin(self(context))
            }),
            accept_validator(),
        )
    }
}

impl<S, C, F, Fut, O> IntoHandler<S, (DirCx<S>, C), O> for F
where
    C: FromCaptures + 'static,
    F: Fn(DirCx<S>, C) -> Fut + 'static,
    Fut: Future<Output = Result<O>> + 'static,
{
    type Context = DirCx<S>;
    type Input = ();

    fn into_handler(self) -> Handler<Self::Context, Self::Input, O> {
        Handler::new(
            Arc::new(move |context: &DirCx<S>, (), captures: Captures| {
                match C::from_captures(&captures) {
                    Ok(parsed) => {
                        let context = DirCx::new((**context).clone(), context.intent().clone());
                        Box::pin(self(context, parsed)) as HandlerFuture<'_, O>
                    },
                    Err(error) => Box::pin(async move { Err(error) }),
                }
            }),
            captures_validator::<C>(),
        )
    }
}

impl<S, C, F, Fut, O> IntoHandler<S, (C, DirCx<S>), O> for F
where
    C: FromCaptures + 'static,
    F: Fn(C, DirCx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<O>> + 'static,
{
    type Context = DirCx<S>;
    type Input = ();

    fn into_handler(self) -> Handler<Self::Context, Self::Input, O> {
        Handler::new(
            Arc::new(move |context: &DirCx<S>, (), captures: Captures| {
                match C::from_captures(&captures) {
                    Ok(parsed) => {
                        let context = DirCx::new((**context).clone(), context.intent().clone());
                        Box::pin(self(parsed, context)) as HandlerFuture<'_, O>
                    },
                    Err(error) => Box::pin(async move { Err(error) }),
                }
            }),
            captures_validator::<C>(),
        )
    }
}

impl<S, F, Fut, O> IntoHandler<S, (Cx<S>,), O> for F
where
    F: Fn(Cx<S>) -> Fut + 'static,
    Fut: Future<Output = Result<O>> + 'static,
{
    type Context = Cx<S>;
    type Input = ();

    fn into_handler(self) -> Handler<Self::Context, Self::Input, O> {
        Handler::new(
            Arc::new(move |context: &Cx<S>, (), _captures: Captures| {
                Box::pin(self(context.clone()))
            }),
            accept_validator(),
        )
    }
}

impl<S, C, F, Fut, O> IntoHandler<S, (Cx<S>, C), O> for F
where
    C: FromCaptures + 'static,
    F: Fn(Cx<S>, C) -> Fut + 'static,
    Fut: Future<Output = Result<O>> + 'static,
{
    type Context = Cx<S>;
    type Input = ();

    fn into_handler(self) -> Handler<Self::Context, Self::Input, O> {
        Handler::new(
            Arc::new(move |context: &Cx<S>, (), captures: Captures| {
                match C::from_captures(&captures) {
                    Ok(parsed) => Box::pin(self(context.clone(), parsed)) as HandlerFuture<'_, O>,
                    Err(error) => Box::pin(async move { Err(error) }),
                }
            }),
            captures_validator::<C>(),
        )
    }
}

/// One row of the dir route table: pattern and normalized handler.
pub(super) struct DirEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: Handler<DirCx<S>, (), DirListing>,
}

pub(super) struct FileEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: Handler<Cx<S>, (), FileProjection>,
    /// The route was declared `ranged`, so its listing/lookup placeholder
    /// projects `ReadMode::Ranged` and the host dispatches `open` straight to
    /// `open-file` without probing. The handler still supplies the real reader,
    /// size, and stability at `open-file`.
    pub(super) ranged: bool,
}

pub(super) struct TreeRefEntry<S> {
    pub(super) pattern: Pattern,
    pub(super) handler: Handler<Cx<S>, (), TreeRef>,
}
