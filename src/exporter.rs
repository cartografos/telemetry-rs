//! Turning a [`Config`] into a tracer provider.

use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{SdkTracerProvider, Sampler};

use crate::config::{Config, Sampling};
use crate::error::{Error, Result};

/// Builds the provider described by `config`.
///
/// Returns `Ok(None)` when the configuration has no endpoint: that is not a
/// failure, it is a service nobody asked to export.
pub(crate) fn provider(config: &Config) -> Result<Option<SdkTracerProvider>> {
    let Some(endpoint) = config.endpoint.as_deref() else {
        return Ok(None);
    };

    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_timeout(config.timeout)
        .with_headers(config.headers.clone())
        .build()
        .map_err(|e| Error::Exporter(e.to_string()))?;

    Ok(Some(
        SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_sampler(sampler(config.sampling))
            .with_resource(resource(config))
            .build(),
    ))
}

/// Parent-based in both cases, which is what keeps one trace whole: a service
/// that decided to record a request does not get contradicted by the next one it
/// calls, and a service that decided not to does not get half a trace exported
/// downstream.
fn sampler(sampling: Sampling) -> Sampler {
    match sampling {
        Sampling::AlwaysOn => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        // Clamped rather than refused: a ratio outside [0, 1] is a typo in an
        // environment variable, and the useful reading of one is "all of it" or
        // "none of it" — not a service that will not start.
        Sampling::Ratio(ratio) => Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            ratio.clamp(0.0, 1.0),
        ))),
    }
}

fn resource(config: &Config) -> Resource {
    let mut attributes = Vec::with_capacity(config.resource_attributes.len() + 2);
    if let Some(version) = &config.service_version {
        attributes.push(KeyValue::new("service.version", version.clone()));
    }
    if let Some(environment) = &config.environment {
        // Semantic conventions 1.27 renamed `deployment.environment` to this.
        // Spelled out rather than taken from the semconv crate, which would be one
        // more version to keep in step for a handful of string constants.
        attributes.push(KeyValue::new(
            "deployment.environment.name",
            environment.clone(),
        ));
    }
    for (key, value) in &config.resource_attributes {
        attributes.push(KeyValue::new(key.clone(), value.clone()));
    }

    Resource::builder()
        .with_service_name(config.service_name.clone())
        .with_attributes(attributes)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_endpoint_is_no_provider_and_no_error() {
        let provider = provider(&Config::new("svc")).expect("no endpoint is not a failure");
        assert!(provider.is_none());
    }

    #[test]
    fn an_endpoint_builds_a_provider() {
        let mut config = Config::new("svc");
        config.endpoint = Some("https://otel.example/v1/traces".to_owned());
        let provider = provider(&config).expect("builds");
        assert!(provider.is_some());
        if let Some(provider) = provider {
            let _ = provider.shutdown();
        }
    }

    /// The exporter is built, not connected: a wrong host is discovered on the
    /// first export and must not stop a service from starting.
    #[test]
    fn an_unreachable_endpoint_still_builds() {
        let mut config = Config::new("svc");
        config.endpoint = Some("https://127.0.0.1:1/v1/traces".to_owned());
        let provider = provider(&config).expect("builds");
        assert!(provider.is_some());
        if let Some(provider) = provider {
            let _ = provider.shutdown();
        }
    }
}
