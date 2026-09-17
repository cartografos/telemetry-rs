# Changelog

Semantic versioning. Consumers pin a tag.

## [0.1.2] — 2026-09-16

- **An empty `RUST_LOG` no longer silences the process.** `EnvFilter`'s own
  `try_from_default_env` parses `""` into a valid filter with no directives, and
  a filter with no directives enables nothing — so a service deployed with
  `RUST_LOG: ${RUST_LOG:-}` in its compose file ran in complete silence while
  answering requests normally. Empty now means absent, as it does everywhere else
  in this crate, and a malformed `RUST_LOG` falls back to `info` with the reason
  on stderr rather than taking the logs down with it.

## [0.1.1] — 2026-09-16

- A failed export now says so. `internal-logs` is enabled on the OTLP exporter
  and the SDK, so a 403, a DNS failure or a rejected payload reaches stdout
  instead of leaving an empty backend that looks exactly like no traffic. Safe
  because those crates' own events were already kept out of the export layer;
  otherwise a failing exporter would try to export the news of it.
- `DEFAULT_FILTERED_TARGETS` covers the hyphenated spellings of the
  OpenTelemetry crate names too. A target is usually `module_path!()` and has
  underscores, but some of these are set by hand to the package name, and one
  missing spelling is one event that closes the loop.

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
