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

use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::{Resource, ResourceBuilder, ResourceDetector};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

const METADATA_ROOT: &str = "http://metadata.google.internal";
const GCE_METADATA_HOST_ENV_VAR: &str = "GCE_METADATA_HOST";
const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_ATTEMPT_COUNT: u32 = 5;
const INSTANCE_METADATA_PATH: &str = "/computeMetadata/v1/instance/";

/// Detects if the application is running in a Google Cloud environment.
#[derive(Clone, Debug)]
pub struct GoogleCloudResourceDetector(Resource);

impl GoogleCloudResourceDetector {
    pub fn builder() -> GoogleCloudResourceDetectorBuilder {
        GoogleCloudResourceDetectorBuilder::new()
    }
}

impl ResourceDetector for GoogleCloudResourceDetector {
    fn detect(&self) -> Resource {
        self.0.clone()
    }
}

#[derive(Debug, Default)]
pub struct GoogleCloudResourceDetectorBuilder {
    endpoint: String,
    attempt_timeout: Duration,
    attempt_count: u32,
    fallback: Option<Resource>,
}

impl GoogleCloudResourceDetectorBuilder {
    fn new() -> Self {
        Self {
            endpoint: std::env::var(GCE_METADATA_HOST_ENV_VAR)
                .unwrap_or_else(|_| METADATA_ROOT.to_string()),
            attempt_count: DEFAULT_ATTEMPT_COUNT,
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
            fallback: None,
        }
    }

    pub async fn build(self) -> Result<GoogleCloudResourceDetector, Error> {
        let resource = self.detect_async().await;
        let resource = match (resource, self.fallback) {
            (Ok(r), _) => r,
            (Err(_), Some(r)) => r,
            (Err(e), None) => return Err(e),
        };
        Ok(GoogleCloudResourceDetector(resource))
    }

    pub async fn detect_async(&self) -> Result<Resource, Error> {
        let builder =
            Resource::builder_empty().with_attribute(KeyValue::new("cloud.provider", "gcp"));
        let builder = if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
            self.gke_resource(builder).await?
        } else if std::env::var("GAE_SERVICE").is_ok() {
            self.gae_resource(builder)
        } else if std::env::var("K_SERVICE").is_ok() {
            self.gcr_resource(builder)
        } else {
            self.gce_resource(builder).await?
        };
        Ok(builder.build())
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.attempt_timeout = timeout;
        self
    }

    pub fn with_attempt_count(mut self, count: u32) -> Self {
        self.attempt_count = count;
        self
    }

    pub fn with_fallback(mut self, fallback: Resource) -> Self {
        self.fallback = Some(fallback);
        self
    }

    async fn gke_resource(&self, builder: ResourceBuilder) -> Result<ResourceBuilder, Error> {
        let text = self.fetch_instance_metadata().await?;
        let instance = serde_json::from_str::<InstanceMetadata>(&text).map_err(Error::mds)?;
        let cluster_name = instance
            .attributes
            .get("cluster-name")
            .map(|v| v.to_string());

        let mut builder = self.gce_resource_impl(builder, instance)?;
        if let Some(v) = cluster_name {
            builder = builder.with_attribute(KeyValue::new("k8s.cluster.name", v));
        }

        Ok(Self::attributes_from_env(
            builder,
            &[
                ("POD_NAME", "k8s.pod.name"),
                ("HOSTNAME", "k8s.pod.name"),
                ("CONTAINER_NAME", "k8s.container.name"),
                ("NAMESPACE_NAME", "k8s.namespace.name"),
            ],
        ))
    }

    async fn gce_resource(&self, builder: ResourceBuilder) -> Result<ResourceBuilder, Error> {
        let text = self.fetch_instance_metadata().await?;
        let instance = serde_json::from_str::<InstanceMetadata>(&text).map_err(Error::mds)?;
        self.gce_resource_impl(builder, instance)
    }

    fn gce_resource_impl<'a>(
        &self,
        builder: ResourceBuilder,
        instance: InstanceMetadata<'a>,
    ) -> Result<ResourceBuilder, Error> {
        // The zone is is projects/{project_id}/zones/{zone_id} format.
        let (region, zone) = if let Some(name) = instance.zone {
            parse_zone(name)
        } else {
            (None, None)
        };

        let builder = builder.with_attributes(
            [
                ("cloud.availability_zone", zone),
                ("cloud.region", region),
                ("gce.instance_id", instance.id),
                ("gce.instance_name", instance.name),
                ("gce.machine_type", instance.machine_type),
            ]
            .into_iter()
            .filter_map(|(k, option)| option.map(|v| (k, v)))
            .map(|(k, v)| KeyValue::new(k, v.to_string())),
        );
        Ok(builder)
    }

    fn gae_resource(&self, builder: ResourceBuilder) -> ResourceBuilder {
        Self::attributes_from_env(
            builder,
            &[
                ("GAE_SERVICE", "gae.service"),
                ("GAE_VERSION", "gae.version"),
                ("GAE_INSTANCE", "gae.instance"),
            ],
        )
    }

    fn gcr_resource(&self, builder: ResourceBuilder) -> ResourceBuilder {
        Self::attributes_from_env(
            builder,
            &[
                ("K_SERVICE", "gcr.service"),
                ("K_REVISION", "gcr.revision"),
                ("K_CONFIGURATION", "gcr.configuration"),
            ],
        )
    }

    fn attributes_from_env(
        builder: ResourceBuilder,
        list: &[(&'static str, &'static str)],
    ) -> ResourceBuilder {
        list.iter().fold(builder, |builder, (var, name)| {
            if let Ok(value) = std::env::var(var) {
                builder.with_attribute(KeyValue::new(*name, value))
            } else {
                builder
            }
        })
    }

    async fn fetch_instance_metadata(&self) -> Result<String, Error> {
        let url = reqwest::Url::parse(&self.endpoint)
            .map_err(Error::url)?
            .join(INSTANCE_METADATA_PATH)
            .map_err(Error::url)?;
        let mut last_error = None;
        for iteration in 0..self.attempt_count {
            if iteration != 0 {
                tokio::time::sleep(self.attempt_timeout).await;
            }
            match self.fetch_metadata_attempt(url.clone()).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap())
    }

    async fn fetch_metadata_attempt(&self, url: reqwest::Url) -> Result<String, Error> {
        reqwest::Client::new()
            .request(reqwest::Method::GET, url)
            .header("Metadata-Flavor", "Google")
            .query(&[("recursive", "true")])
            .timeout(self.attempt_timeout)
            .send()
            .await
            .map_err(Error::mds)?
            .error_for_status()
            .map_err(Error::mds)?
            .text()
            .await
            .map_err(Error::mds)
    }
}

#[derive(Debug, serde::Deserialize)]
struct InstanceMetadata<'a> {
    zone: Option<&'a str>,
    id: Option<&'a str>,
    name: Option<&'a str>,
    #[serde(rename = "machine-type")]
    machine_type: Option<&'a str>,
    #[serde(borrow)]
    attributes: BTreeMap<&'a str, &'a str>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("cannot parse endpoint: {0:?}")]
    Url(#[source] BoxedError),
    #[error("cannot retrieve data from metadata server: {0:?}")]
    Mds(#[source] BoxedError),
    #[error("cannot parse data received from metadata server: {0:?}")]
    Parse(#[source] BoxedError),
}

impl Error {
    fn url<E>(error: E) -> Self
    where
        E: Into<BoxedError>,
    {
        Error::Url(error.into())
    }

    fn mds<E>(error: E) -> Self
    where
        E: Into<BoxedError>,
    {
        Error::Mds(error.into())
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync + 'static>;

fn parse_zone(zone: &str) -> (Option<&str>, Option<&str>) {
    let parts: Vec<&str> = zone.split('/').collect();
    let id = match &parts[..] {
        ["projects", _, "zones", zone_id] => *zone_id,
        _ => return (None, None),
    };
    let parts: Vec<&str> = id.split('-').collect();
    match &parts[..] {
        [_geo, _region, letter] if !letter.is_empty() => {
            (Some(&id[0..(id.len() - letter.len() - 1)]), Some(id))
        }
        _ => (None, Some(id)),
    }
}

#[derive(Clone, Debug)]
pub struct GenericNodeDetector {
    id: String,
    location: String,
    namespace: String,
}

impl GenericNodeDetector {
    pub fn new() -> Self {
        let id = Uuid::new_v4().to_string();
        Self {
            id,
            location: "us-central1".to_string(),
            namespace: "google-cloud-rust".to_string(),
        }
    }
}

impl Default for GenericNodeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceDetector for GenericNodeDetector {
    fn detect(&self) -> Resource {
        Resource::builder_empty()
            .with_attributes([
                KeyValue::new("location", self.location.clone()),
                KeyValue::new("namespace", self.namespace.clone()),
                KeyValue::new("node_id", self.id.clone()),
            ])
            .build()
    }
}
