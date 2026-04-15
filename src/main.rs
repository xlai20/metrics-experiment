// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod detector;

use anyhow::Result;
use detector::{GenericNodeDetector, GoogleCloudResourceDetector};
use google_cloud_auth::credentials::{CacheableResource, Credentials, EntityTag};
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::metrics::{
    Aggregation, Instrument, InstrumentKind, SdkMeterProvider, Stream,
};
use opentelemetry_sdk::resource::ResourceDetector;
use std::time::Duration;
use tokio::sync::watch;
use tonic::metadata::MetadataMap;
use tonic::service::Interceptor;
use tonic::{Request, Status};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const GCP_OTLP_ENDPOINT: &str = "https://telemetry.googleapis.com";
const OTEL_KEY_GCP_PROJECT_ID: &str = "gcp.project_id";
const OTEL_KEY_SERVICE_NAME: &str = "service.name";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let project_id =
        std::env::var("GOOGLE_CLOUD_PROJECT").unwrap_or_else(|_| "xlai-sdk-project".to_string());

    let node = GenericNodeDetector::new();
    let detector = GoogleCloudResourceDetector::builder()
        .with_fallback(node.detect())
        .build()
        .await?;

    // 1. Initialize the Metrics Provider using the Builder pattern
    let provider = Builder::new(&project_id, "xlai-metrics-experiment")
        .with_detector(node.clone())
        .build()
        .await
        .expect("failed to build provider");

    // 2. Create a Histogram Metric
    let meter = provider.meter("xlai-metrics-experiment-meter");
    let histogram = meter
        .f64_histogram("xlai-metrics-experiment.histogram")
        .with_unit("s")
        .build();

    // 3. Record some data
    println!("Recording metrics for project: {}", project_id);
    let attributes = [KeyValue::new(
        "xlai_metrics_experiment_id",
        uuid::Uuid::new_v4().to_string(),
    )];
    for i in 1..=10 {
        histogram.record(i as f64 * 0.1, &attributes);
    }

    // 4. Force flush and shutdown
    println!("Flushing metrics...");
    provider.force_flush()?; // This one throws an error.
    println!("Done.");

    Ok(())
}

/// Creates a `SdkMeterProvider` optimized for Google Cloud Monitoring.
pub struct Builder {
    project_id: String,
    service_name: String,
    credentials: Option<Credentials>,
    endpoint: http::Uri,
    detector: Option<Box<dyn ResourceDetector>>,
}

impl Builder {
    /// Creates a new builder with the required Google Cloud project ID and service name.
    pub fn new<P, S>(project_id: P, service_name: S) -> Self
    where
        P: Into<String>,
        S: Into<String>,
    {
        let uri = http::Uri::from_static(GCP_OTLP_ENDPOINT);
        Self {
            project_id: project_id.into(),
            service_name: service_name.into(),
            credentials: None,
            endpoint: uri,
            detector: None,
        }
    }

    /// Sets the credentials used for authentication.
    pub fn with_credentials(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Sets a custom OTLP endpoint.
    pub fn with_endpoint(mut self, uri: http::Uri) -> Self {
        self.endpoint = uri;
        self
    }

    /// Sets the resource detector.
    pub fn with_detector<D>(mut self, detector: D) -> Self
    where
        D: ResourceDetector + 'static,
    {
        self.detector = Some(Box::new(detector));
        self
    }

    /// Builds and initializes the `SdkMeterProvider`.
    pub async fn build(self) -> Result<SdkMeterProvider> {
        let resource = opentelemetry_sdk::Resource::builder()
            .with_attributes(vec![
                KeyValue::new(OTEL_KEY_GCP_PROJECT_ID, self.project_id.clone()),
                KeyValue::new(OTEL_KEY_SERVICE_NAME, self.service_name.clone()),
            ])
            .with_detectors(&Vec::from_iter(self.detector.into_iter()))
            .build();

        tracing::info!(
            "Initializing SdkMeterProvider with resource: {:?}",
            resource
        );

        let credentials = match self.credentials {
            Some(c) => c,
            None => google_cloud_auth::credentials::Builder::default().build()?,
        };
        let interceptor = CloudTelemetryAuthInterceptor::new(credentials).await;

        let exporter_builder = {
            let builder = opentelemetry_otlp::MetricExporter::builder()
                .with_tonic()
                .with_endpoint(self.endpoint.to_string())
                .with_interceptor(interceptor);

            if self
                .endpoint
                .scheme()
                .is_none_or(|s| s != &http::uri::Scheme::HTTPS)
            {
                builder
            } else {
                let domain = self
                    .endpoint
                    .authority()
                    .ok_or_else(|| anyhow::anyhow!("invalid URI"))?;
                let config = ClientTlsConfig::new()
                    .with_enabled_roots()
                    .domain_name(domain.host());
                builder.with_tls_config(config)
            }
        };

        let exporter = exporter_builder.build()?;
        let view = move |ins: &Instrument| {
            let name = if Self::name_missing_prefix(ins.name()) {
                format!("workload.googleapis.com/{}", ins.name())
            } else {
                ins.name().to_string()
            };
            let builder = Stream::builder().with_name(name);
            let builder = if ins.kind() != InstrumentKind::Histogram {
                builder
            } else {
                builder.with_aggregation(Aggregation::Base2ExponentialHistogram {
                    max_size: 32,
                    max_scale: 20,
                    record_min_max: true,
                })
            };
            builder.build().expect("stream should be valid").into()
        };

        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .with_resource(resource)
            .with_view(view)
            .build();

        global::set_meter_provider(provider.clone());
        Ok(provider)
    }

    /// Returns true if the metric name needs a `workload.googleapis.com` prefix.
    fn name_missing_prefix(name: &str) -> bool {
        const W: &str = "workload.googleapis.com";
        const C: &str = "custom.googleapis.com";
        const P: &str = "projects";
        const D: &str = "metricDescriptors";
        let mut s = name.split('/');
        !matches!(
            (s.next(), s.next(), s.next(), s.next(), s.next()),
            (Some(P), Some(_), Some(D), Some(W), Some(_))
                | (Some(P), Some(_), Some(D), Some(C), Some(_))
                | (Some(W), Some(_), _, _, _)
                | (Some(C), Some(_), _, _, _)
        )
    }
}

/// A Tonic interceptor that injects Google Cloud authentication headers.
/// Mimics `o11y` reference implementation with ETag-based polling.
#[derive(Clone)]
pub struct CloudTelemetryAuthInterceptor {
    rx: watch::Receiver<Option<MetadataMap>>,
}

impl CloudTelemetryAuthInterceptor {
    pub async fn new(credentials: Credentials) -> Self {
        let (tx, mut rx) = watch::channel(None);
        tokio::spawn(refresh_task(credentials, tx));

        // Wait for the first refresh to complete.
        if rx.borrow().is_none() {
            let _ = rx.changed().await;
        }
        Self { rx }
    }
}

impl Interceptor for CloudTelemetryAuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let rx_ref = self.rx.borrow();
        let metadata = rx_ref.as_ref().ok_or_else(|| {
            tracing::error!("GCP credentials unavailable in interceptor");
            Status::unauthenticated("GCP credentials unavailable")
        })?;

        for entry in metadata.iter() {
            match entry {
                tonic::metadata::KeyAndValueRef::Ascii(key, value) => {
                    request.metadata_mut().insert(key.clone(), value.clone());
                }
                tonic::metadata::KeyAndValueRef::Binary(key, value) => {
                    request
                        .metadata_mut()
                        .insert_bin(key.clone(), value.clone());
                }
            }
        }
        Ok(request)
    }
}

async fn refresh_task(credentials: Credentials, tx: watch::Sender<Option<MetadataMap>>) {
    let mut last_etag: Option<EntityTag> = None;
    loop {
        let mut extensions = http::Extensions::new();
        if let Some(etag) = last_etag.clone() {
            extensions.insert(etag);
        }

        match credentials.headers(extensions).await {
            Ok(CacheableResource::New { entity_tag, data }) => {
                let mut metadata = MetadataMap::new();
                for (name, value) in data.iter() {
                    if let (Ok(key), Ok(mut val)) = (
                        tonic::metadata::MetadataKey::from_bytes(name.as_str().as_bytes()),
                        tonic::metadata::MetadataValue::try_from(value.as_bytes()),
                    ) {
                        val.set_sensitive(value.is_sensitive());
                        metadata.insert(key, val);
                    }
                }

                if tx.send(Some(metadata)).is_err() {
                    tracing::warn!("Auth refresh task: receiver dropped, stopping");
                    break;
                }
                last_etag = Some(entity_tag);
                tracing::debug!(
                    "Auth refreshed successfully, next refresh in {:?}",
                    REFRESH_INTERVAL
                );
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
            Ok(CacheableResource::NotModified) => {
                tracing::debug!(
                    "Auth not modified, checking again in {:?}",
                    REFRESH_INTERVAL
                );
                tokio::time::sleep(REFRESH_INTERVAL).await;
            }
            Err(e) => {
                tracing::error!("Auth refresh failed: {:?}", e);
                if e.is_transient() {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                } else {
                    tracing::error!("Auth refresh failed (fatal): {:?}", e);
                    break;
                }
            }
        }
    }
}
