//! Carrying a trace across a service boundary, over W3C `traceparent`.
//!
//! One rule decides the shape of everything here: **an invalid context is never
//! written**. A process with no exporter installed has no real span, so
//! [`inject_context`] adds no header at all and the request it forwards is
//! byte-for-byte the request it received. Telemetry that changes the wire when it
//! is switched off is telemetry that has to be explained in someone else's test.

use http::HeaderMap;
use opentelemetry::Context;
use opentelemetry::global;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Writes the current span's trace context into `headers`.
///
/// Call it on an outgoing request — a proxied request, a client call — right
/// before it leaves. Does nothing when there is no valid context to write.
///
/// ```no_run
/// # use http::HeaderMap;
/// let mut headers = HeaderMap::new();
/// cartografo_telemetry::inject_context(&mut headers);
/// // `traceparent` is now set, if this process is tracing at all.
/// ```
pub fn inject_context(headers: &mut HeaderMap) {
    inject_context_from(&Span::current(), headers);
}

/// [`inject_context`] for a span other than the current one.
pub fn inject_context_from(span: &Span, headers: &mut HeaderMap) {
    let context = span.context();
    if !context.span().span_context().is_valid() {
        return;
    }
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderInjector(headers));
    });
}

/// Reads the trace context an upstream service put on the request.
///
/// `None` when there is no `traceparent`, or when the one there is does not
/// parse. Anyone can send the header, so a caller should treat the result as a
/// link offered rather than a fact established: adopting it makes this span a
/// child, and adopting nothing makes it a root, which is the correct outcome for
/// a request that arrived from outside.
pub fn extract_context(headers: &HeaderMap) -> Option<Context> {
    let context =
        global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)));
    context.span().span_context().is_valid().then_some(context)
}

/// The current trace's id as the 32 lowercase hex characters a backend searches
/// by, or `None` when this process is not tracing.
///
/// Worth putting on an error response or a support page: it is the one string
/// that turns "it failed at about four" into one trace.
pub fn current_trace_id() -> Option<String> {
    let context = Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guarantee the gateway's differential harness depends on: with no
    /// telemetry installed, nothing is added to a forwarded request.
    #[test]
    fn nothing_is_injected_without_a_span() {
        let mut headers = HeaderMap::new();
        inject_context(&mut headers);
        assert!(
            headers.is_empty(),
            "an uninstrumented process must forward exactly what it received"
        );
    }

    #[test]
    fn an_absent_traceparent_extracts_to_nothing() {
        assert!(extract_context(&HeaderMap::new()).is_none());
    }

    #[test]
    fn a_malformed_traceparent_extracts_to_nothing() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "not-a-trace-context".parse().expect("header"),
        );
        assert!(
            extract_context(&headers).is_none(),
            "a caller controls this header; an unparseable one makes a root span, not an error"
        );
    }

    /// All-zero ids are the "invalid" span context the W3C spec defines. Adopting
    /// one would put every such request into a single trace that means nothing.
    #[test]
    fn an_all_zero_traceparent_is_invalid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-00000000000000000000000000000000-0000000000000000-01"
                .parse()
                .expect("header"),
        );
        assert!(extract_context(&headers).is_none());
    }

    #[test]
    fn no_trace_id_without_telemetry() {
        assert!(current_trace_id().is_none());
    }
}
