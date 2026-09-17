//! What to export, where, and how the same process logs to its terminal.
//!
//! Everything here has an environment variable behind it, and the OpenTelemetry
//! ones are spelled exactly as the [OTLP specification][spec] spells them, so a
//! service configured through this crate reads the same way as one configured
//! through any other SDK. What the builder sets explicitly wins over the
//! environment, because a value in code is a decision and a value in the
//! environment is a default.
//!
//! [spec]: https://opentelemetry.io/docs/specs/otel/protocol/exporter/

use std::collections::HashMap;
use std::env;
use std::time::Duration;

use crate::error::{Error, Result};

/// How the process writes to its own stdout, independently of what it exports.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    /// One line per event, for a person reading `docker logs`.
    #[default]
    Text,
    /// One JSON object per event, for a log aggregator.
    Json,
    /// Nothing on stdout. Only for a process whose logs are somebody else's
    /// problem — with no exporter configured either, it is silent.
    Off,
}

/// Which spans are recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Sampling {
    /// Every span. The right answer for a service whose traffic is small enough
    /// to keep, which is most internal services.
    #[default]
    AlwaysOn,
    /// A share of the traces, between 0.0 and 1.0, decided once per trace by its
    /// id and respected by every service that sees it.
    ///
    /// Parent-based: a request that arrives already sampled stays sampled, so a
    /// trace is never half recorded.
    Ratio(f64),
}

/// The resolved configuration a [`crate::Telemetry`] is built from.
#[derive(Clone, Debug)]
pub struct Config {
    /// `service.name` on every span. The one attribute every backend groups by.
    pub service_name: String,
    /// `service.version`, when the service knows its own.
    pub service_version: Option<String>,
    /// `deployment.environment.name`: production, staging, development.
    pub environment: Option<String>,
    /// Where spans are POSTed. `None` disables export entirely.
    pub endpoint: Option<String>,
    /// Sent with every export request.
    pub headers: HashMap<String, String>,
    /// How long one export attempt may take.
    pub timeout: Duration,
    /// Which spans are recorded.
    pub sampling: Sampling,
    /// Extra resource attributes, applied after the ones above.
    pub resource_attributes: Vec<(String, String)>,
    /// The `EnvFilter` directives used when `RUST_LOG` is unset.
    pub default_filter: String,
    /// How the process writes to stdout.
    pub log_format: LogFormat,
    /// Targets whose events never reach the exporter.
    ///
    /// The exporter speaks HTTP over reqwest over rustls, and each of those emits
    /// `tracing` events of its own. Feeding those back into the layer that
    /// produced them is a loop that exports the act of exporting. These are the
    /// crates that close it; they still reach stdout, where they are diagnostics
    /// somebody may need.
    pub export_filtered_targets: Vec<String>,
}

/// The default export timeout, as the OTLP specification defines it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Crate names whose own logs must not be exported. See
/// [`Config::export_filtered_targets`].
pub const DEFAULT_FILTERED_TARGETS: [&str; 9] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "reqwest",
    "rustls",
    "hyper",
    "hyper_util",
    "h2",
    "tokio_util",
];

impl Config {
    /// A configuration for `service_name` with nothing read from the environment.
    ///
    /// Export is off: [`Config::endpoint`] is `None` until something sets it.
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            service_version: None,
            environment: None,
            endpoint: None,
            headers: HashMap::new(),
            timeout: DEFAULT_TIMEOUT,
            sampling: Sampling::default(),
            resource_attributes: Vec::new(),
            default_filter: "info".to_owned(),
            log_format: LogFormat::default(),
            export_filtered_targets: DEFAULT_FILTERED_TARGETS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }

    /// Applies the environment on top of `self`, leaving anything the environment
    /// does not mention untouched.
    ///
    /// Reads, in the order they are resolved:
    ///
    /// | Variable | Effect |
    /// |---|---|
    /// | `OTEL_SERVICE_NAME` | overrides the service name |
    /// | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | the full URL, taken verbatim |
    /// | `OTEL_EXPORTER_OTLP_ENDPOINT` | the base URL; `/v1/traces` is appended |
    /// | `OTEL_EXPORTER_OTLP_HEADERS` | `key=value,key=value` |
    /// | `CF_ACCESS_CLIENT_ID` + `CF_ACCESS_CLIENT_SECRET` | the two Cloudflare Access headers |
    /// | `OTEL_EXPORTER_OTLP_TIMEOUT` | milliseconds |
    /// | `OTEL_TRACES_SAMPLER_ARG` | the sampling ratio |
    /// | `OTEL_RESOURCE_ATTRIBUTES` | `key=value,key=value` |
    /// | `DEPLOYMENT_ENVIRONMENT`, else `APP_ENV` | the environment name |
    ///
    /// An empty value counts as absent. That is not pedantry: `FOO: ${FOO:-}` in
    /// a compose file reaches the container as an empty string rather than as an
    /// unset name, and a configuration that read it as present would point an
    /// exporter at `/v1/traces` on no host at all.
    pub fn with_env(mut self) -> Result<Self> {
        if let Some(name) = var("OTEL_SERVICE_NAME") {
            self.service_name = name;
        }
        if let Some(endpoint) = traces_endpoint() {
            self.endpoint = Some(endpoint);
        }
        if let Some(raw) = var("OTEL_EXPORTER_OTLP_HEADERS") {
            for (key, value) in parse_pairs(&raw, "OTEL_EXPORTER_OTLP_HEADERS")? {
                self.headers.insert(key, value);
            }
        }
        // Cloudflare Access, as the two headers its policies check. Both or
        // neither: one alone is refused exactly like none, and sending half a pair
        // only makes the 403 harder to read.
        if let (Some(id), Some(secret)) =
            (var("CF_ACCESS_CLIENT_ID"), var("CF_ACCESS_CLIENT_SECRET"))
        {
            self.headers.insert("CF-Access-Client-Id".to_owned(), id);
            self.headers
                .insert("CF-Access-Client-Secret".to_owned(), secret);
        }
        if let Some(raw) = var("OTEL_EXPORTER_OTLP_TIMEOUT") {
            let millis: u64 = raw.parse().map_err(|_| {
                Error::Config(format!(
                    "OTEL_EXPORTER_OTLP_TIMEOUT is milliseconds as an integer, got {raw:?}"
                ))
            })?;
            self.timeout = Duration::from_millis(millis);
        }
        if let Some(raw) = var("OTEL_TRACES_SAMPLER_ARG") {
            let ratio: f64 = raw.parse().map_err(|_| {
                Error::Config(format!(
                    "OTEL_TRACES_SAMPLER_ARG is a number between 0 and 1, got {raw:?}"
                ))
            })?;
            self.sampling = Sampling::Ratio(ratio);
        }
        if let Some(raw) = var("OTEL_RESOURCE_ATTRIBUTES") {
            self.resource_attributes
                .extend(parse_pairs(&raw, "OTEL_RESOURCE_ATTRIBUTES")?);
        }
        if let Some(environment) = var("DEPLOYMENT_ENVIRONMENT").or_else(|| var("APP_ENV")) {
            self.environment = Some(environment);
        }
        Ok(self)
    }

    /// Whether this configuration exports anything at all.
    pub fn is_exporting(&self) -> bool {
        self.endpoint.is_some()
    }
}

/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` verbatim, else
/// `OTEL_EXPORTER_OTLP_ENDPOINT` with the signal path appended.
///
/// Both halves are what the OTLP specification requires. Appending the path here
/// rather than leaving it to the exporter is what makes the address readable in
/// one place on the day the collector answers 404 to a path nobody chose.
fn traces_endpoint() -> Option<String> {
    if let Some(exact) = var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        return Some(exact);
    }
    let base = var("OTEL_EXPORTER_OTLP_ENDPOINT")?;
    Some(join_signal_path(&base))
}

/// Appends `/v1/traces` unless the URL already ends in it.
pub(crate) fn join_signal_path(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1/traces") {
        return trimmed.to_owned();
    }
    format!("{trimmed}/v1/traces")
}

fn var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// `key=value,key=value`, the encoding both `OTEL_EXPORTER_OTLP_HEADERS` and
/// `OTEL_RESOURCE_ATTRIBUTES` use.
///
/// A value may itself contain `=` (a base64 secret usually ends in one), so only
/// the FIRST `=` separates. A pair with no `=` is an error rather than a silent
/// skip: a header that was meant to be sent and was not is the hardest kind of
/// misconfiguration to see from the outside.
fn parse_pairs(raw: &str, source: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (key, value) = item.split_once('=').ok_or_else(|| {
            Error::Config(format!(
                "{source} is key=value,key=value; {item:?} has no '='"
            ))
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::Config(format!("{source} has an entry with no key")));
        }
        pairs.push((key.to_owned(), value.trim().to_owned()));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signal_path_is_appended_once() {
        assert_eq!(
            join_signal_path("https://otel.example"),
            "https://otel.example/v1/traces"
        );
        assert_eq!(
            join_signal_path("https://otel.example/"),
            "https://otel.example/v1/traces",
            "a trailing slash must not double up: //v1/traces is a 404"
        );
        assert_eq!(
            join_signal_path("https://otel.example/v1/traces"),
            "https://otel.example/v1/traces",
            "a base that already names the signal is left alone"
        );
    }

    #[test]
    fn a_value_may_contain_the_separator() {
        let pairs = parse_pairs("authorization=Basic dXNlcjpwYXNz==", "T").expect("parses");
        assert_eq!(
            pairs,
            vec![(
                "authorization".to_owned(),
                "Basic dXNlcjpwYXNz==".to_owned()
            )],
            "only the first '=' separates, or every base64 secret loses its padding"
        );
    }

    #[test]
    fn several_pairs_and_stray_whitespace() {
        let pairs = parse_pairs(" a=1 , b=2 ,", "T").expect("parses");
        assert_eq!(
            pairs,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }

    #[test]
    fn a_pair_without_a_separator_is_refused() {
        let err = parse_pairs("just-a-word", "T").expect_err("is an error");
        assert!(
            err.to_string().contains("just-a-word"),
            "the error has to name the entry, or it cannot be found: {err}"
        );
    }

    #[test]
    fn a_new_config_exports_nothing() {
        let config = Config::new("svc");
        assert!(!config.is_exporting());
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(config.sampling, Sampling::AlwaysOn);
    }
}
