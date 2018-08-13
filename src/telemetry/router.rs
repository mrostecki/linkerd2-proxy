use std::{
    default::Default,
    fmt,
    sync::{Arc, Mutex, Weak},
};

use ctx;
use control::destination::QueryCounter;
use telemetry::metrics::{Counter, Gauge, Scopes, Direction};

metrics! {
    router_active_destination_queries: Gauge {
        "Current number of active Destination service queries."
    },
    router_error_total: Counter {
        "Total number of router errors."
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sensors {
    active_destination_queries: QueryCounter,
    error_total: Arc<ErrorTotalInner>,
}

type ErrorTotalInner = Mutex<Scopes<ErrorLabels, Counter>>;

/// Formats metrics for Prometheus for a corresponding set of router `Sensors`.
#[derive(Debug, Default)]
pub struct Report {
    active_destination_queries: QueryCounter,
    error_total: Weak<ErrorTotalInner>,
}

/// Sensor for recording error total metrics.
///
/// When this type is dropped, its metrics may no longer be formatted for prometheus.
#[derive(Clone, Debug)]
pub struct ErrorSensor {
    inner: Arc<ErrorTotalInner>,
    direction: Direction,
}

#[derive(Debug, Eq, PartialEq, Hash)]
enum ErrorKind {
    Route,
    Capacity,
    NotRecognized,
    Inner,
}

#[derive(Debug, Eq, PartialEq, Hash)]
struct ErrorLabels {
    direction: Direction,
    kind: ErrorKind,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ErrorKind::Route => f.pad("kind=\"route\""),
            ErrorKind::Capacity => f.pad("kind=\"at_capacity\""),
            ErrorKind::NotRecognized => f.pad("kind=\"route_not_recognized\""),
            ErrorKind::Inner => f.pad("kind=\"inner\""),
        }
    }
}

impl fmt::Display for ErrorLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{}", self.direction, self.kind)
    }
}


// ===== impl Sensors =====

impl Sensors {
    pub fn new() -> Self {
        Self {

            ..Default::default()
        }
    }

    pub fn error_total(&self, proxy_ctx: &ctx::Proxy) -> ErrorSensor {
        ErrorSensor {
            inner: self.error_total.clone(),
            direction: Direction::from_context(proxy_ctx),
        }
    }

    pub fn query_counter(&self) -> &QueryCounter {
        &self.active_destination_queries
    }

    pub fn report(&self) -> Report {
        Report {
            active_destination_queries: self.active_destination_queries.clone(),
            error_total: Arc::downgrade(&self.error_total),
        }
    }
}


// ===== impl ErrorSensor =====

impl ErrorSensor {
    pub fn route_not_recognized(&self) {
        if let Ok(mut scopes) = self.inner.lock() {
            let labels = ErrorLabels {
                direction: self.direction,
                kind: ErrorKind::NotRecognized,
            };
            scopes.get_or_default(labels).incr();
        }
    }

    pub fn at_capacity(&self) {
        if let Ok(mut scopes) = self.inner.lock() {
            let labels = ErrorLabels {
                direction: self.direction,
                kind: ErrorKind::Capacity,
            };
            scopes.get_or_default(labels).incr();
        }
    }

    pub fn route_error(&self) {
        if let Ok(mut scopes) = self.inner.lock() {
            let labels = ErrorLabels {
                direction: self.direction,
                kind: ErrorKind::Route,
            };
            scopes.get_or_default(labels).incr();
        }
    }

    pub fn inner_error(&self) {
        // TODO: It would be good to have more information about these errors.
        if let Ok(mut scopes) = self.inner.lock() {
            let labels = ErrorLabels {
                direction: self.direction,
                kind: ErrorKind::Inner,
            };
            scopes.get_or_default(labels).incr();
        }
    }
}

// ===== impl Report =====

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {

        if let Some(lock) = self.error_total.upgrade() {
            if let Ok(error_total) = lock.lock() {
                router_error_total.fmt_help(f)?;
                router_error_total.fmt_scopes(f, &*error_total, |s| &s)?;
            }
        }

        let queries = Gauge::from(
            self.active_destination_queries
                .active_queries() as u64
        );
        router_active_destination_queries.fmt_help(f)?;
        router_active_destination_queries.fmt_metric(f, queries)?;

        Ok(())
    }
}