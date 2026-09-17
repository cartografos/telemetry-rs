//! The span an HTTP request gets: a `tower-http` layer any axum, hyper or tower
//! service can mount.
//!
//! Field names are the OpenTelemetry [HTTP semantic conventions][semconv] 1.27
//! exactly — `http.request.method`, `url.path`, `http.response.status_code` — so
//! a backend's built-in charts find them. A well-meant `method` or `status`
//! would draw nothing at all.
//!
//! [semconv]: https://opentelemetry.io/docs/specs/semconv/http/http-spans/

use std::time::Duration;

use http::{Request, Response};
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::{
    DefaultOnBodyChunk, DefaultOnEos, DefaultOnFailure, DefaultOnRequest, MakeSpan, OnResponse,
    TraceLayer,
};
use tracing::field::Empty;
use tracing::{Span, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::propagate::extract_context;

/// The layer's full type, named so a caller can put it in a `ServiceBuilder`
/// whose type it spells out.
pub type HttpTraceLayer = TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    HttpSpan,
    DefaultOnRequest,
    HttpStatus,
    DefaultOnBodyChunk,
    DefaultOnEos,
    DefaultOnFailure,
>;

/// The tracing layer, with every default.
///
/// ```
/// // Mounted like any other tower layer — on an axum `Router`, a
/// // `ServiceBuilder`, or a hyper service.
/// let layer = cartografo_telemetry::http::trace_layer();
/// ```
pub fn trace_layer() -> HttpTraceLayer {
    trace_layer_with(HttpSpan::new())
}

/// [`trace_layer`] over a span builder that was configured.
pub fn trace_layer_with(span: HttpSpan) -> HttpTraceLayer {
    TraceLayer::new_for_http()
        .make_span_with(span)
        .on_response(HttpStatus)
}

/// Builds the server span for each request.
#[derive(Clone, Copy, Debug)]
pub struct HttpSpan {
    adopt_inbound_context: bool,
    normalize_names: bool,
    record_query: bool,
}

impl Default for HttpSpan {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSpan {
    /// Adopts an inbound `traceparent`, normalizes span names, and leaves the
    /// query string off the span.
    pub fn new() -> Self {
        Self {
            adopt_inbound_context: true,
            normalize_names: true,
            record_query: false,
        }
    }

    /// Ignores `traceparent` on incoming requests: every request starts a new
    /// trace.
    ///
    /// For a service on the open internet whose callers are not part of the same
    /// system — an inbound header is attacker-controlled, and adopting it lets
    /// anyone choose which trace their request lands in.
    pub fn without_inbound_context(mut self) -> Self {
        self.adopt_inbound_context = false;
        self
    }

    /// Uses the request path as the span name, identifiers and all.
    ///
    /// The default replaces them ([`operation_name`]) because a backend's list of
    /// operations is built from span names, and one name per row turns it into a
    /// log. Turn it off when the paths are already templates.
    pub fn with_raw_span_names(mut self) -> Self {
        self.normalize_names = false;
        self
    }

    /// Records the query string as `url.query`.
    ///
    /// **Off by default, and think before turning it on.** A path addresses a
    /// resource; a query string carries values somebody typed — filters, dates,
    /// search terms, sometimes a token. A trace backend keeps what it is sent for
    /// as long as its retention says, and that is a poor place to accumulate them.
    pub fn with_query(mut self) -> Self {
        self.record_query = true;
        self
    }
}

impl<B> MakeSpan<B> for HttpSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        let method = request.method().clone();
        let path = request.uri().path();
        let name = if self.normalize_names {
            operation_name(method.as_str(), path)
        } else {
            format!("{method} {path}")
        };

        // `otel.*` are tracing-opentelemetry's control fields rather than
        // attributes: they set the exported span's name, kind and status.
        let span = info_span!(
            "http.request",
            otel.name = %name,
            otel.kind = "server",
            otel.status_code = Empty,
            http.request.method = %method,
            url.path = %path,
            url.query = Empty,
            http.response.status_code = Empty,
        );
        if self.record_query
            && let Some(query) = request.uri().query()
        {
            span.record("url.query", query);
        }
        if self.adopt_inbound_context
            && let Some(parent) = extract_context(request.headers())
        {
            // Both ways this can fail are benign and neither is worth a line per
            // request: the span is disabled, so it exports nothing to parent; or
            // it has already been entered, which cannot happen to one built here.
            let _ = span.set_parent(parent);
        }
        span
    }
}

/// Records the status on the span once the response is ready.
#[derive(Clone, Copy, Debug, Default)]
pub struct HttpStatus;

impl<B> OnResponse<B> for HttpStatus {
    fn on_response(self, response: &Response<B>, latency: Duration, span: &Span) {
        let status = response.status();
        span.record("http.response.status_code", status.as_u16());
        // Only 5xx marks the span as failed. A 401 or a 404 is a service
        // answering correctly, and colouring those red is how an error rate stops
        // meaning anything.
        if status.is_server_error() {
            span.record("otel.status_code", "ERROR");
        }
        tracing::debug!(
            status = status.as_u16(),
            latency_ms = latency.as_millis() as u64,
            "response"
        );
    }
}

/// `METHOD /path/with/{id}/replaced` — the span's name.
///
/// Three shapes of identifier are recognised, which between them cover what a
/// REST path actually carries: a serial id (`/expenses/412`), a uuid, and an
/// opaque token, including one with a file extension (`/<token>.pdf`). A short
/// word that happens to be hexadecimal is a word: `card-cut` survives, and so
/// does `dec0de`.
pub fn operation_name(method: &str, path: &str) -> String {
    let mut out = String::with_capacity(method.len() + path.len() + 1);
    out.push_str(method);
    out.push(' ');
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        if is_identifier(segment) {
            out.push_str("{id}");
        } else {
            out.push_str(segment);
        }
    }
    if out.ends_with(' ') {
        out.push('/');
    }
    out
}

/// Whether a path segment names one row rather than one kind of thing.
fn is_identifier(segment: &str) -> bool {
    // The extension is cut before the length is judged, not after: a document key
    // is `<token>.<ext>` and the token is what decides.
    let stem = segment.split('.').next().unwrap_or(segment);
    if stem.is_empty() {
        return false;
    }
    if stem.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let hexish = stem
        .bytes()
        .all(|b| b.is_ascii_hexdigit() || b == b'-' || b == b'_');
    hexish && stem.len() >= 16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collection_keeps_its_name() {
        assert_eq!(
            operation_name("GET", "/api/v1/expenses"),
            "GET /api/v1/expenses"
        );
    }

    #[test]
    fn a_serial_id_becomes_a_parameter() {
        assert_eq!(
            operation_name("PUT", "/api/v1/card-payments/412/status"),
            "PUT /api/v1/card-payments/{id}/status"
        );
    }

    #[test]
    fn a_uuid_becomes_a_parameter() {
        assert_eq!(
            operation_name("GET", "/accounts/6f1b4c2e-0d3a-4f5b-8c7d-9e0a1b2c3d4e"),
            "GET /accounts/{id}"
        );
    }

    #[test]
    fn an_opaque_key_becomes_a_parameter_extension_and_all() {
        assert_eq!(
            operation_name("GET", "/control/9f86d081884c7d659a2feaa0c55ad015.pdf"),
            "GET /control/{id}"
        );
    }

    /// The length rule is the only thing standing between a real route and a span
    /// called `{id}`: `card-cut` and `dec0de` are both valid hexadecimal.
    #[test]
    fn a_short_hex_word_is_a_word() {
        assert_eq!(
            operation_name("GET", "/api/v1/reports/card-cut"),
            "GET /api/v1/reports/card-cut"
        );
        assert_eq!(
            operation_name("GET", "/api/v1/dec0de"),
            "GET /api/v1/dec0de"
        );
    }

    #[test]
    fn the_root_is_a_path_and_not_an_empty_name() {
        assert_eq!(operation_name("GET", "/"), "GET /");
        assert_eq!(operation_name("GET", ""), "GET /");
    }

    #[test]
    fn a_hashed_asset_keeps_its_name() {
        assert_eq!(
            operation_name("GET", "/static/js/index.a1b2c3z9.js"),
            "GET /static/js/index.a1b2c3z9.js"
        );
    }

    #[test]
    fn repeated_slashes_collapse_rather_than_producing_empty_segments() {
        assert_eq!(operation_name("GET", "//api//v1//"), "GET /api/v1");
    }

    #[test]
    fn the_defaults_are_the_conservative_ones() {
        let span = HttpSpan::new();
        assert!(span.adopt_inbound_context);
        assert!(span.normalize_names);
        assert!(
            !span.record_query,
            "a query string is opt-in: it carries values, not addresses"
        );
    }
}
