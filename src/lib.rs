//! OpenTelemetry tracing for a Rust service, in one call.
//!
//! ```no_run
//! let telemetry = cartografo_telemetry::Telemetry::builder("my-service")
//!     .version(env!("CARGO_PKG_VERSION"))
//!     .default_filter("my_service=info,tower_http=info")
//!     .install();
//!
//! // … serve …
//!
//! // Spans are batched, so the last of them only leave on the way out.
//! telemetry.shutdown();
//! ```
//!
//! # Nothing on the wire until an endpoint is configured
//!
//! With `OTEL_EXPORTER_OTLP_ENDPOINT` unset, [`Builder::install`] gives the
//! process exactly the `tracing` subscriber it would have had otherwise: no
//! exporter, no background thread, no `traceparent` on an outgoing request. A
//! service links this crate and stays unchanged until somebody deploys it with a
//! collector to talk to.
//!
//! That is a guarantee and not an accident. Telemetry that alters the wire when
//! it is switched off is telemetry that has to be explained inside somebody
//! else's test.
//!
//! # Two services, one trace
//!
//! [`http::trace_layer`] extracts W3C `traceparent` on the way in and
//! [`inject_context`] writes it on the way out. A gateway that injects and a
//! backend that extracts produce one trace with two spans; either half alone
//! produces two traces and no error anywhere, which is why both live in one
//! crate rather than in each service. Both need the `http` feature, which is on
//! by default; [`inject_context`] shows the outgoing half.
//!
//! # What must not be exported
//!
//! Span attributes are addresses and outcomes: methods, paths, ids, status codes,
//! durations. Not values. A trace backend keeps what it is sent, for as long as
//! its retention says, and usually somewhere with a broader audience than the
//! database the values came from. The defaults here take that seriously — the
//! query string is off unless [`http::HttpSpan::with_query`] turns it on.
//!
//! # Configuration
//!
//! Everything is settable on the [`Builder`] and everything has an environment
//! variable behind it, spelled as the OTLP specification spells it. See
//! [`Config::with_env`] for the full table.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod config;
mod error;
mod exporter;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http")]
mod propagate;

use std::collections::HashMap;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub use config::{Config, DEFAULT_FILTERED_TARGETS, DEFAULT_TIMEOUT, LogFormat, Sampling};
pub use error::{Error, Result};

#[cfg(feature = "http")]
pub use propagate::{current_trace_id, extract_context, inject_context, inject_context_from};

// Re-exported so a service can reach the OpenTelemetry API without adding — and
// then having to keep in step — a second copy of these crates in its own
// manifest. Two versions of `opentelemetry` in one binary do not conflict in a
// way cargo reports; they produce "expected Tracer, found Tracer" with two
// identical-looking paths.
pub use opentelemetry;
pub use opentelemetry_sdk;
pub use tracing_opentelemetry;

/// A live telemetry installation. Hold it for the life of the process and
/// [`shutdown`](Telemetry::shutdown) it on the way out.
///
/// Dropping it without shutting down loses whatever the batch processor still
/// holds — which, for a process that exits early, is most of what it recorded.
#[must_use = "dropping this loses the spans still in the batch; call shutdown()"]
#[derive(Debug)]
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
    service_name: String,
    endpoint: Option<String>,
}

impl Telemetry {
    /// Starts configuring telemetry for a service.
    ///
    /// `OTEL_SERVICE_NAME` overrides `service_name`; what is passed here is the
    /// name the binary knows itself by.
    pub fn builder(service_name: impl Into<String>) -> Builder {
        Builder {
            config: Config::new(service_name),
            read_env: true,
        }
    }

    /// An installation that exports nothing and holds nothing — for a test, or
    /// for a caller that decided against telemetry after building one.
    pub fn disabled() -> Self {
        Self {
            provider: None,
            service_name: String::new(),
            endpoint: None,
        }
    }

    /// Whether spans are actually leaving this process.
    pub fn is_exporting(&self) -> bool {
        self.provider.is_some()
    }

    /// The name this service reports itself under.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Where spans are being sent, if anywhere.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Exports whatever is batched right now, without stopping.
    ///
    /// For a long quiet process, or just before something that might not come
    /// back — an abort, a migration, an exec.
    pub fn force_flush(&self) -> Result<()> {
        match &self.provider {
            Some(provider) => provider
                .force_flush()
                .map_err(|e| Error::Shutdown(e.to_string())),
            None => Ok(()),
        }
    }

    /// Flushes and stops the exporter, reporting what went wrong.
    pub fn try_shutdown(self) -> Result<()> {
        match self.provider {
            Some(provider) => provider
                .shutdown()
                .map_err(|e| Error::Shutdown(e.to_string())),
            None => Ok(()),
        }
    }

    /// [`try_shutdown`](Telemetry::try_shutdown), complaining on stderr instead
    /// of returning.
    ///
    /// stderr and not `tracing::error!`: the subscriber is being torn down around
    /// this call, so it is the one place the message is certain to be readable.
    pub fn shutdown(self) {
        if let Err(e) = self.try_shutdown() {
            eprintln!("telemetry: {e}");
        }
    }
}

/// Configures and installs telemetry. Built by [`Telemetry::builder`].
///
/// Anything set here wins over the environment, which is read first: a value in
/// code is a decision, a value in the environment is a default.
#[derive(Clone, Debug)]
pub struct Builder {
    config: Config,
    read_env: bool,
}

impl Builder {
    /// Sets `service.version`.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.config.service_version = Some(version.into());
        self
    }

    /// Sets `deployment.environment.name` — production, staging, development.
    pub fn environment(mut self, environment: impl Into<String>) -> Self {
        self.config.environment = Some(environment.into());
        self
    }

    /// Sets the collector's base URL; `/v1/traces` is appended if it is not
    /// already there. Setting it is what turns export on.
    pub fn endpoint(mut self, endpoint: impl AsRef<str>) -> Self {
        self.config.endpoint = Some(config::join_signal_path(endpoint.as_ref()));
        self
    }

    /// Adds one header to every export request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.config.headers.insert(name.into(), value.into());
        self
    }

    /// The Cloudflare Access service token, as the two headers its policies
    /// check.
    ///
    /// For a collector published behind Access: without these it answers 403 with
    /// an HTML login page, which an OTLP client reports as a protocol error
    /// rather than as "you are not authenticated".
    pub fn cloudflare_access(
        self,
        client_id: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        self.header("CF-Access-Client-Id", client_id)
            .header("CF-Access-Client-Secret", secret)
    }

    /// Records this share of traces, between 0.0 and 1.0. Parent-based, so a
    /// trace is never half recorded. Out-of-range values are clamped.
    pub fn sample_ratio(mut self, ratio: f64) -> Self {
        self.config.sampling = Sampling::Ratio(ratio);
        self
    }

    /// How long one export attempt may take. Default [`DEFAULT_TIMEOUT`].
    pub fn export_timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    /// Adds a resource attribute to every span.
    pub fn resource_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.config
            .resource_attributes
            .push((key.into(), value.into()));
        self
    }

    /// The `EnvFilter` directives to use when `RUST_LOG` is unset.
    pub fn default_filter(mut self, filter: impl Into<String>) -> Self {
        self.config.default_filter = filter.into();
        self
    }

    /// How the process writes to stdout. Default [`LogFormat::Text`].
    pub fn log_format(mut self, format: LogFormat) -> Self {
        self.config.log_format = format;
        self
    }

    /// Ignores the environment entirely: only what this builder sets applies.
    pub fn without_env(mut self) -> Self {
        self.read_env = false;
        self
    }

    /// The configuration as it stands, environment included. Useful to log, or
    /// to assert on in a test.
    pub fn config(&self) -> Result<Config> {
        if self.read_env {
            // The environment is the DEFAULT, so it is applied to a fresh config
            // and then overwritten by whatever this builder was told explicitly.
            let from_env = Config::new(self.config.service_name.clone()).with_env()?;
            Ok(self.config.clone().over(from_env))
        } else {
            Ok(self.config.clone())
        }
    }

    /// Installs telemetry, reporting what went wrong.
    ///
    /// Fails when the configuration is unusable, when the exporter cannot be
    /// built, or when a global `tracing` subscriber is already installed.
    pub fn try_install(self) -> Result<Telemetry> {
        let config = self.config()?;
        let provider = exporter::provider(&config)?;

        let directives = config::with_diagnostics(config::directives(
            config::rust_log(),
            &config.default_filter,
        ));
        let filter = EnvFilter::try_new(&directives).unwrap_or_else(|e| {
            // A malformed RUST_LOG must not silence a service either. stderr
            // because the subscriber this filter belongs to is not installed yet.
            eprintln!("telemetry: RUST_LOG {directives:?} is not a filter ({e}); using info");
            EnvFilter::new("info")
        });

        let logs = match config.log_format {
            LogFormat::Text => Some(tracing_subscriber::fmt::layer().boxed()),
            LogFormat::Json => Some(tracing_subscriber::fmt::layer().json().boxed()),
            LogFormat::Off => None,
        };

        let exported = provider.as_ref().map(|provider| {
            let targets = config.export_filtered_targets.clone();
            // Applied to the OTLP layer ONLY. The same events still reach stdout,
            // where they are diagnostics somebody may need; what they must not do
            // is reach the exporter that produced them.
            let no_feedback = tracing_subscriber::filter::filter_fn(move |meta| {
                let root = meta.target().split("::").next().unwrap_or_default();
                !targets.iter().any(|t| t == root)
            });
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("cartografo-telemetry"))
                .with_filter(no_feedback)
                .boxed()
        });

        if let Err(_already_set) = tracing_subscriber::registry()
            .with(filter)
            .with(logs)
            .with(exported)
            .try_init()
        {
            // The provider owns an exporter thread. Leaving it running behind a
            // subscriber nothing feeds would be a thread that exports nothing
            // forever.
            if let Some(provider) = provider {
                let _ = provider.shutdown();
            }
            return Err(Error::SubscriberAlreadySet);
        }

        if let Some(provider) = &provider {
            // W3C `traceparent`, which is what joins one service's span to the
            // next one's. Set after the subscriber so the line below is the first
            // thing the new one records.
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            // Also as the global provider, so a caller using the OpenTelemetry API
            // directly — rather than through `tracing` — reaches the same one.
            opentelemetry::global::set_tracer_provider(provider.clone());
            tracing::info!(
                service = %config.service_name,
                endpoint = %config.endpoint.clone().unwrap_or_default(),
                environment = %config.environment.clone().unwrap_or_default(),
                "exporting traces"
            );
        }

        Ok(Telemetry {
            provider,
            service_name: config.service_name,
            endpoint: config.endpoint,
        })
    }

    /// Installs telemetry, and carries on without it if it cannot.
    ///
    /// A service that cannot reach its collector is still a service: the
    /// application is the product and the traces are the view of it. The reason
    /// goes to stderr — the subscriber is not installed yet, so there is nowhere
    /// else it would reliably appear — and the returned [`Telemetry`] is inert.
    ///
    /// Use [`try_install`](Builder::try_install) where the failure should be the
    /// caller's to handle.
    pub fn install(self) -> Telemetry {
        match self.try_install() {
            Ok(telemetry) => telemetry,
            Err(e) => {
                eprintln!("telemetry: {e}; continuing without traces");
                Telemetry::disabled()
            }
        }
    }
}

impl Config {
    /// `self` laid over `base`: every field this config states explicitly wins,
    /// and the rest is taken from `base`.
    fn over(self, base: Self) -> Self {
        let mut headers: HashMap<String, String> = base.headers;
        headers.extend(self.headers);
        let mut resource_attributes = base.resource_attributes;
        resource_attributes.extend(self.resource_attributes);

        let defaults = Config::new(String::new());
        Self {
            // The service name reaches `base` already (the builder seeds it), so
            // `base` is the one that saw `OTEL_SERVICE_NAME`.
            service_name: base.service_name,
            service_version: self.service_version.or(base.service_version),
            environment: self.environment.or(base.environment),
            endpoint: self.endpoint.or(base.endpoint),
            headers,
            timeout: pick(self.timeout, base.timeout, defaults.timeout),
            sampling: pick(self.sampling, base.sampling, defaults.sampling),
            resource_attributes,
            default_filter: if self.default_filter == defaults.default_filter {
                base.default_filter
            } else {
                self.default_filter
            },
            log_format: pick(self.log_format, base.log_format, defaults.log_format),
            export_filtered_targets: self.export_filtered_targets,
        }
    }
}

/// `mine` unless it is still the untouched default, in which case `theirs`.
fn pick<T: PartialEq>(mine: T, theirs: T, default: T) -> T {
    if mine == default { theirs } else { mine }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builder_without_an_endpoint_exports_nothing() {
        let config = Telemetry::builder("svc")
            .without_env()
            .config()
            .expect("configures");
        assert!(!config.is_exporting());
    }

    #[test]
    fn an_endpoint_gets_the_signal_path() {
        let config = Telemetry::builder("svc")
            .without_env()
            .endpoint("https://otel.example")
            .config()
            .expect("configures");
        assert_eq!(
            config.endpoint.as_deref(),
            Some("https://otel.example/v1/traces")
        );
    }

    #[test]
    fn cloudflare_access_is_the_two_headers() {
        let config = Telemetry::builder("svc")
            .without_env()
            .cloudflare_access("id.access", "secret")
            .config()
            .expect("configures");
        assert_eq!(
            config
                .headers
                .get("CF-Access-Client-Id")
                .map(String::as_str),
            Some("id.access")
        );
        assert_eq!(
            config
                .headers
                .get("CF-Access-Client-Secret")
                .map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn a_disabled_installation_shuts_down_without_complaint() {
        let telemetry = Telemetry::disabled();
        assert!(!telemetry.is_exporting());
        assert!(telemetry.force_flush().is_ok());
        telemetry.try_shutdown().expect("nothing to stop");
    }

    /// What the builder states must survive the environment being laid under it.
    #[test]
    fn explicit_values_win_over_the_environment() {
        let base = Config {
            endpoint: Some("https://from-env/v1/traces".to_owned()),
            environment: Some("from-env".to_owned()),
            ..Config::new("svc")
        };
        let explicit = Config {
            endpoint: Some("https://explicit/v1/traces".to_owned()),
            ..Config::new("svc")
        };
        let merged = explicit.over(base);
        assert_eq!(
            merged.endpoint.as_deref(),
            Some("https://explicit/v1/traces")
        );
        assert_eq!(
            merged.environment.as_deref(),
            Some("from-env"),
            "what the builder did not state still comes from the environment"
        );
    }
}
