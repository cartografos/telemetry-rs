# cartografo-telemetry

OpenTelemetry tracing for a Rust service, in one call. Traces over OTLP/HTTP, a
`tower-http` layer for HTTP spans, and W3C context propagation so two services
produce one trace.

Built for the Cartógrafo services (SigNoz behind a Cloudflare tunnel), but there
is nothing specific to them in here: the collector, the credentials and the
environment all come from configuration.

```toml
[dependencies]
cartografo-telemetry = { git = "https://github.com/cartografos/telemetry-rs", tag = "v0.1.0" }
```

```rust
fn main() {
    let telemetry = cartografo_telemetry::Telemetry::builder("my-service")
        .version(env!("CARGO_PKG_VERSION"))
        .default_filter("my_service=info,tower_http=info")
        .install();

    // … serve …

    // Spans are batched, so the last of them only leave on the way out.
    telemetry.shutdown();
}
```

```rust
use axum::Router;

let app = Router::new().layer(cartografo_telemetry::http::trace_layer());
```

## The three things it guarantees

**Nothing on the wire until an endpoint is configured.** With
`OTEL_EXPORTER_OTLP_ENDPOINT` unset, `install()` leaves the process with exactly
the `tracing` subscriber it would have had otherwise: no exporter, no background
thread, no `traceparent` on an outgoing request. A service can link this crate
and stay byte-identical until somebody deploys it with a collector to talk to.

**Two services, one trace.** `http::trace_layer()` extracts `traceparent` on the
way in; `inject_context()` writes it on the way out. Both halves live here
because either alone produces two disconnected traces and no error anywhere.

```rust
let mut headers = http::HeaderMap::new();
cartografo_telemetry::inject_context(&mut headers); // the outgoing request joins this trace
```

**A failure to export is never a failure to serve.** `install()` reports the
problem on stderr and returns an inert handle. Use `try_install()` where the
caller wants the error.

## Configuration

Everything is settable on the builder, and everything has an environment
variable behind it, spelled as the OTLP specification spells it. What the
builder states wins; the environment is the default.

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | collector base URL; `/v1/traces` is appended. **Setting it is what turns export on.** |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | the full URL, taken verbatim |
| `OTEL_SERVICE_NAME` | overrides the name passed to `builder()` |
| `OTEL_EXPORTER_OTLP_HEADERS` | `key=value,key=value` on every export request |
| `CF_ACCESS_CLIENT_ID` + `CF_ACCESS_CLIENT_SECRET` | the two Cloudflare Access headers |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | milliseconds per export attempt (default 10000) |
| `OTEL_TRACES_SAMPLER_ARG` | sampling ratio, 0.0–1.0 (default: record everything) |
| `OTEL_RESOURCE_ATTRIBUTES` | `key=value,key=value` on every span |
| `DEPLOYMENT_ENVIRONMENT`, else `APP_ENV` | `deployment.environment.name` |
| `RUST_LOG` | the `EnvFilter`, as always; `default_filter()` applies when it is unset |

An empty value counts as absent — `FOO: ${FOO:-}` in a compose file arrives as
an empty string, and reading that as "configured" points the exporter at
`/v1/traces` on no host at all.

## What it puts on a span

Span names are `METHOD /path/with/{id}/replaced`, because a backend's list of
operations is built from span names and one row per id turns it into a log. The
real path stays on `url.path`.

Attributes are the HTTP semantic conventions 1.27 — `http.request.method`,
`url.path`, `http.response.status_code` — so a backend's built-in charts find
them. Only 5xx marks a span as failed: a 401 or a 404 is a service answering
correctly.

**The query string is off by default.** A path addresses a resource; a query
string carries values somebody typed. `HttpSpan::with_query()` turns it on where
that is genuinely wanted.

## Features

| Feature | Default | What it brings |
|---|---|---|
| `http` | yes | the `tower-http` layer and the propagation helpers |

Without `http`, the crate is the exporter and the subscriber alone — what a
worker or a CLI wants.

## Transport

OTLP over **HTTP/protobuf**, never gRPC. HTTP is what survives a reverse proxy,
a Cloudflare tunnel and a TLS terminator; gRPC needs end-to-end HTTP/2 and a port
that is rarely the one published. TLS uses rustls with the Mozilla root store
compiled in, so a scratch or distroless runtime image needs no `ca-certificates`
for a span to leave it.

## Versioning

Semantic versioning, tagged `vMAJOR.MINOR.PATCH`. Consumers pin a tag rather
than a branch — `tag = "v0.1.0"` — so a change here never lands in another
service's build without somebody moving the tag there.

## License

MIT.
