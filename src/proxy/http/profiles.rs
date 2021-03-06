extern crate tower_discover;

use futures::Stream;
use http;
use indexmap::IndexMap;
use regex::Regex;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::iter::FromIterator;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use tower_retry::budget::Budget;

use never::Never;

use NameAddr;

pub type Routes = Vec<(RequestMatch, Route)>;

/// Watches a destination's Routes.
///
/// The stream updates with all routes for the given destination. The stream
/// never ends and cannot fail.
pub trait GetRoutes {
    type Stream: Stream<Item = Routes, Error = Never>;

    fn get_routes(&self, dst: &NameAddr) -> Option<Self::Stream>;
}

/// Implemented by target types that may be combined with a Route.
pub trait WithRoute {
    type Output;

    fn with_route(self, route: Route) -> Self::Output;
}

/// Implemented by target types that may have a `NameAddr` destination that
/// can be discovered via `GetRoutes`.
pub trait CanGetDestination {
    fn get_destination(&self) -> Option<&NameAddr>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Route {
    labels: Labels,
    response_classes: ResponseClasses,
    retries: Option<Retries>,
    timeout: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum RequestMatch {
    All(Vec<RequestMatch>),
    Any(Vec<RequestMatch>),
    Not(Box<RequestMatch>),
    Path(Regex),
    Method(http::Method),
}

#[derive(Clone, Debug)]
pub struct ResponseClass {
    is_failure: bool,
    match_: ResponseMatch,
}

#[derive(Clone, Default)]
pub struct ResponseClasses(Arc<Vec<ResponseClass>>);

#[derive(Clone, Debug)]
pub enum ResponseMatch {
    All(Vec<ResponseMatch>),
    Any(Vec<ResponseMatch>),
    Not(Box<ResponseMatch>),
    Status {
        min: http::StatusCode,
        max: http::StatusCode,
    },
}

#[derive(Clone, Debug)]
pub struct Retries {
    budget: Arc<Budget>,
}

#[derive(Clone, Default)]
struct Labels(Arc<IndexMap<String, String>>);

// === impl Route ===

impl Route {
    pub fn new<I>(label_iter: I, response_classes: Vec<ResponseClass>) -> Self
    where
        I: Iterator<Item = (String, String)>,
    {
        let labels = {
            let mut pairs = label_iter.collect::<Vec<_>>();
            pairs.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));
            Labels(Arc::new(IndexMap::from_iter(pairs)))
        };

        Self {
            labels,
            response_classes: ResponseClasses(response_classes.into()),
            retries: None,
            timeout: None,
        }
    }

    pub fn labels(&self) -> &Arc<IndexMap<String, String>> {
        &self.labels.0
    }

    pub fn response_classes(&self) -> &ResponseClasses {
        &self.response_classes
    }

    pub fn retries(&self) -> Option<&Retries> {
        self.retries.as_ref()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    pub fn set_retries(&mut self, budget: Arc<Budget>) {
        self.retries = Some(Retries { budget });
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }
}

// === impl RequestMatch ===

impl RequestMatch {
    fn is_match<B>(&self, req: &http::Request<B>) -> bool {
        match self {
            RequestMatch::Method(ref method) => req.method() == *method,
            RequestMatch::Path(ref re) => re.is_match(req.uri().path()),
            RequestMatch::Not(ref m) => !m.is_match(req),
            RequestMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            RequestMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
        }
    }
}

// === impl ResponseClass ===

impl ResponseClass {
    pub fn new(is_failure: bool, match_: ResponseMatch) -> Self {
        Self { is_failure, match_ }
    }

    pub fn is_failure(&self) -> bool {
        self.is_failure
    }

    pub fn is_match<B>(&self, req: &http::Response<B>) -> bool {
        self.match_.is_match(req)
    }
}

// === impl ResponseClasses ===

impl Deref for ResponseClasses {
    type Target = [ResponseClass];

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl PartialEq for ResponseClasses {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ResponseClasses {}

impl Hash for ResponseClasses {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.0) as *const _ as usize);
    }
}

impl fmt::Debug for ResponseClasses {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl ResponseMatch ===

impl ResponseMatch {
    fn is_match<B>(&self, req: &http::Response<B>) -> bool {
        match self {
            ResponseMatch::Status { ref min, ref max } => {
                *min <= req.status() && req.status() <= *max
            }
            ResponseMatch::Not(ref m) => !m.is_match(req),
            ResponseMatch::All(ref ms) => ms.iter().all(|m| m.is_match(req)),
            ResponseMatch::Any(ref ms) => ms.iter().any(|m| m.is_match(req)),
        }
    }
}

// === impl Retries ===

impl Retries {
    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }
}

impl PartialEq for Retries {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.budget, &other.budget)
    }
}

impl Eq for Retries {}

impl Hash for Retries {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.budget) as *const _ as usize);
    }
}

// === impl Labels ===

impl PartialEq for Labels {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Labels {}

impl Hash for Labels {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(Arc::as_ref(&self.0) as *const _ as usize);
    }
}

impl fmt::Debug for Labels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A stack module that produces a Service that routes requests through alternate
/// middleware configurations
///
/// As the router's Stack is built, a destination is extracted from the stack's
/// target and it is used to get route profiles from ` GetRoutes` implemetnation.
///
/// Each route uses a shared underlying stack. As such, it assumed that the
/// underlying stack is buffered, and so `poll_ready` is NOT called on the routes
/// before requests are dispatched. If an individual route wishes to apply
/// backpressure, it must implement its own buffer/limit strategy.
pub mod router {
    extern crate linkerd2_router as rt;

    use futures::{Async, Poll, Stream};
    use http;
    use std::hash::Hash;

    use never::Never;

    use dns;
    use svc;

    use super::*;

    type Error = Box<dyn std::error::Error + Send + Sync>;

    pub fn layer<T, G, M, R, B>(
        suffixes: Vec<dns::Suffix>,
        get_routes: G,
        route_layer: R,
    ) -> Layer<G, M, R, B>
    where
        T: CanGetDestination + WithRoute + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        G: GetRoutes + Clone,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
    {
        Layer {
            suffixes,
            get_routes,
            route_layer,
            default_route: Route::default(),
            _p: ::std::marker::PhantomData,
        }
    }

    #[derive(Debug)]
    pub struct Layer<G, M, R, B> {
        get_routes: G,
        route_layer: R,
        suffixes: Vec<dns::Suffix>,
        /// This is saved into a field so that the same `Arc`s are used and
        /// cloned, instead of calling `Route::default()` every time.
        default_route: Route,
        _p: ::std::marker::PhantomData<fn() -> (M, B)>,
    }

    #[derive(Debug)]
    pub struct Stack<G, M, R, B> {
        inner: M,
        get_routes: G,
        route_layer: R,
        suffixes: Vec<dns::Suffix>,
        default_route: Route,
        _p: ::std::marker::PhantomData<fn(B)>,
    }

    pub struct Service<G, T, R, B>
    where
        T: WithRoute + Clone,
        T::Output: Eq + Hash,
        R: svc::Stack<T::Output>,
        R::Value: svc::Service<http::Request<B>> + Clone,
    {
        target: T,
        stack: R,
        route_stream: Option<G>,
        router: Router<B, T, R>,
        default_route: Route,
    }

    type Router<B, T, M> = rt::Router<http::Request<B>, Recognize<T>, M>;

    pub struct Recognize<T> {
        target: T,
        routes: Routes,
        default_route: Route,
    }

    impl<B, T> rt::Recognize<http::Request<B>> for Recognize<T>
    where
        T: WithRoute + Clone,
        T::Output: Eq + Hash,
    {
        type Target = T::Output;

        fn recognize(&self, req: &http::Request<B>) -> Option<Self::Target> {
            for (ref condition, ref route) in &self.routes {
                if condition.is_match(&req) {
                    trace!("using configured route: {:?}", condition);
                    return Some(self.target.clone().with_route(route.clone()));
                }
            }

            trace!("using default route");
            Some(self.target.clone().with_route(self.default_route.clone()))
        }
    }

    impl<T, G, M, R, B> svc::Layer<T, T, M> for Layer<G, M, R, B>
    where
        T: CanGetDestination + WithRoute + Clone,
        <T as WithRoute>::Output: Eq + Hash,
        G: GetRoutes + Clone,
        M: svc::Stack<T>,
        M::Value: Clone,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
        R::Stack: Clone,
        <R::Stack as svc::Stack<<T as WithRoute>::Output>>::Value:
            svc::Service<http::Request<B>> + Clone,
    {
        type Value = <Stack<G, M, R, B> as svc::Stack<T>>::Value;
        type Error = <Stack<G, M, R, B> as svc::Stack<T>>::Error;
        type Stack = Stack<G, M, R, B>;

        fn bind(&self, inner: M) -> Self::Stack {
            Stack {
                inner,
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                suffixes: self.suffixes.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<G, M, R, B> Clone for Layer<G, M, R, B>
    where
        G: Clone,
        R: Clone,
    {
        fn clone(&self) -> Self {
            Layer {
                suffixes: self.suffixes.clone(),
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<T, G, M, R, B> svc::Stack<T> for Stack<G, M, R, B>
    where
        T: CanGetDestination + WithRoute + Clone,
        <T as WithRoute>::Output: Eq + Hash,
        M: svc::Stack<T>,
        M::Value: Clone,
        G: GetRoutes,
        R: svc::Layer<
                <T as WithRoute>::Output,
                <T as WithRoute>::Output,
                svc::shared::Stack<M::Value>,
            > + Clone,
        R::Stack: Clone,
        <R::Stack as svc::Stack<<T as WithRoute>::Output>>::Value:
            svc::Service<http::Request<B>> + Clone,
    {
        type Value = Service<G::Stream, T, R::Stack, B>;
        type Error = M::Error;

        fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
            let inner = self.inner.make(&target)?;
            let stack = self.route_layer.bind(svc::shared::stack(inner));

            let router = Router::new(
                Recognize {
                    target: target.clone(),
                    routes: Vec::new(),
                    default_route: self.default_route.clone(),
                },
                stack.clone(),
                // only need 1 for default_route at first
                1,
                // Doesn't matter, since we are guaranteed to have enough capacity.
                Duration::from_secs(0),
            );

            let route_stream = match target.get_destination() {
                Some(ref dst) => {
                    if self.suffixes.iter().any(|s| s.contains(dst.name())) {
                        debug!("fetching routes for {:?}", dst);
                        self.get_routes.get_routes(&dst)
                    } else {
                        debug!("skipping route discovery for dst={:?}", dst);
                        None
                    }
                }
                None => {
                    debug!("no destination for routes");
                    None
                }
            };

            Ok(Service {
                target: target.clone(),
                stack,
                route_stream,
                router,
                default_route: self.default_route.clone(),
            })
        }
    }

    impl<G, M, R, B> Clone for Stack<G, M, R, B>
    where
        G: Clone,
        M: Clone,
        R: Clone,
    {
        fn clone(&self) -> Self {
            Stack {
                inner: self.inner.clone(),
                get_routes: self.get_routes.clone(),
                route_layer: self.route_layer.clone(),
                suffixes: self.suffixes.clone(),
                default_route: self.default_route.clone(),
                _p: ::std::marker::PhantomData,
            }
        }
    }

    impl<G, T, R, B> Service<G, T, R, B>
    where
        G: Stream<Item = Routes, Error = Never>,
        T: WithRoute + Clone,
        T::Output: Eq + Hash,
        R: svc::Stack<T::Output> + Clone,
        R::Value: svc::Service<http::Request<B>> + Clone,
    {
        fn update_routes(&mut self, routes: Routes) {
            let slots = routes.len() + 1;
            self.router = Router::new(
                Recognize {
                    target: self.target.clone(),
                    routes,
                    default_route: self.default_route.clone(),
                },
                self.stack.clone(),
                slots,
                // Doesn't matter, since we are guaranteed to have enough capacity.
                Duration::from_secs(0),
            );
        }

        fn poll_route_stream(&mut self) -> Option<Async<Option<Routes>>> {
            self.route_stream
                .as_mut()
                .and_then(|ref mut s| s.poll().ok())
        }
    }

    impl<G, T, Stk, B, Svc> svc::Service<http::Request<B>> for Service<G, T, Stk, B>
    where
        G: Stream<Item = Routes, Error = Never>,
        T: WithRoute + Clone,
        T::Output: Eq + Hash,
        Stk: svc::Stack<T::Output, Value = Svc> + Clone,
        Stk::Error: Into<Error>,
        Svc: svc::Service<http::Request<B>> + Clone,
        Svc::Error: Into<Error>,
    {
        type Response = Svc::Response;
        type Error = Error;
        type Future = rt::ResponseFuture<http::Request<B>, Svc>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            while let Some(Async::Ready(Some(routes))) = self.poll_route_stream() {
                self.update_routes(routes);
            }

            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            self.router.call(req)
        }
    }
}
