// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Mock source plugin descriptor and configuration DTOs.

use crate::{DataType, MockSourceBuilder};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

fn default_sensor_count() -> u32 {
    5
}

/// Type of data to generate from the mock source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[schema(as = source::mock::DataType)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum DataTypeDto {
    Counter,
    SensorReading {
        #[serde(default = "default_sensor_count", rename = "sensorCount")]
        sensor_count: u32,
    },
    #[default]
    Generic,
}

/// Mock source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mock::MockSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockSourceConfigDto {
    #[serde(default)]
    #[schema(value_type = source::mock::DataType)]
    pub data_type: DataTypeDto,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: ConfigValue<u64>,
}

fn default_interval_ms() -> ConfigValue<u64> {
    ConfigValue::Static(5000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(MockSourceConfigDto, DataTypeDto,)))]
struct MockSourceSchemas;

/// Descriptor for the mock source plugin.
pub struct MockSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MockSourceDescriptor {
    fn kind(&self) -> &str {
        "mock"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.mock.MockSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MockSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: MockSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let data_type = match &dto.data_type {
            DataTypeDto::Counter => DataType::Counter,
            DataTypeDto::SensorReading { sensor_count } => DataType::SensorReading {
                sensor_count: *sensor_count,
            },
            DataTypeDto::Generic => DataType::Generic,
        };

        let interval_ms: u64 = mapper.resolve_typed(&dto.interval_ms)?;

        let source = MockSourceBuilder::new(id)
            .with_data_type(data_type)
            .with_interval_ms(interval_ms)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
