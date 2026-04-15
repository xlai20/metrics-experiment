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
use opentelemetry_sdk::resource::{Resource, ResourceDetector};
use uuid::Uuid;

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
