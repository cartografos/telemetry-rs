//! What can go wrong, and what a caller can do about each one.

/// The result of anything in this crate that can fail.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure while installing or tearing down telemetry.
///
/// Every variant is recoverable by the caller: a service that cannot export
/// traces is still a service, which is why [`crate::Builder::install`] swallows
/// these and [`crate::Builder::try_install`] hands them over instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The OTLP exporter could not be constructed — a malformed endpoint, a
    /// header value that is not valid ASCII, a TLS backend that failed to start.
    #[error("building the OTLP exporter: {0}")]
    Exporter(String),

    /// A `tracing` subscriber was already installed globally.
    ///
    /// Usually a second [`crate::Builder::install`] in one process, or a test
    /// harness that set one up first. The telemetry that is already running
    /// keeps running; this call did nothing.
    #[error("a global tracing subscriber is already installed")]
    SubscriberAlreadySet,

    /// A configured value could not be used as given.
    #[error("{0}")]
    Config(String),

    /// The exporter did not stop cleanly. Spans still in the batch may not have
    /// arrived.
    #[error("shutting telemetry down: {0}")]
    Shutdown(String),
}
