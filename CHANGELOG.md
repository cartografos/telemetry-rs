# Changelog

Semantic versioning. Consumers pin a tag.

## [0.1.0] — 2026-09-16

First release.

- `Telemetry::builder(name).install()`: OTLP/HTTP span export, a `tracing`
  subscriber (text or JSON), and a guard that flushes the batch on shutdown.
- Configuration from the builder and from the OTLP environment variables, with
  the builder winning. Empty values count as absent.
- Cloudflare Access service tokens as first-class configuration, for a collector
  published behind Access.
- `http::trace_layer()`: a `tower-http` layer producing server spans under the
  HTTP semantic conventions 1.27, with identifiers replaced in span names and the
  query string left off unless asked for.
- `inject_context` / `extract_context`: W3C `traceparent` in both directions, and
  never written when there is no valid context — a process with no endpoint
  configured forwards exactly what it received.
- Parent-based sampling, always-on by default, ratio via
  `OTEL_TRACES_SAMPLER_ARG`.

Not here yet, and deliberately: **logs** and **metrics** over OTLP. Both are a
second export pipeline with their own failure modes, and shipping them untested
alongside traces would make this crate harder to trust rather than more useful.
