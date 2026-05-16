// Copyright 2026 The Drasi Authors.
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

//! Descriptor for the dashboard reaction plugin.

use chrono::Utc;
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::storage::{DashboardConfig, DashboardWidget, GridOptions, WidgetGrid};
use crate::DashboardReactionBuilder;

// ---------------------------------------------------------------------------
// DTO types for predefined dashboard configuration
// ---------------------------------------------------------------------------

/// Grid layout options for a predefined dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::GridOptions)]
#[serde(rename_all = "camelCase")]
pub struct GridOptionsDto {
    /// Number of grid columns (default: 12).
    #[serde(default = "default_columns")]
    pub columns: u32,

    /// Height of each grid row in pixels (default: 60).
    #[serde(default = "default_row_height")]
    pub row_height: u32,

    /// Margin between widgets in pixels (default: 10).
    #[serde(default = "default_margin")]
    pub margin: u32,
}

fn default_columns() -> u32 {
    12
}
fn default_row_height() -> u32 {
    60
}
fn default_margin() -> u32 {
    10
}

/// Grid position and size for a widget.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::WidgetGrid)]
pub struct WidgetGridDto {
    /// Column position (0-based).
    pub x: u32,
    /// Row position (0-based).
    pub y: u32,
    /// Width in grid columns.
    pub w: u32,
    /// Height in grid rows.
    pub h: u32,
}

/// Supported widget types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::WidgetType)]
#[serde(rename_all = "snake_case")]
pub enum WidgetTypeDto {
    /// Line chart — track trends over time.
    LineChart,
    /// Bar chart — compare values across categories.
    BarChart,
    /// Pie chart — show proportions of a whole.
    PieChart,
    /// Table — tabular data display.
    Table,
    /// Gauge — radial gauge for a single metric.
    Gauge,
    /// KPI — large single-value indicator.
    Kpi,
    /// Text — Handlebars markdown template.
    Text,
    /// Map — geographic map visualization.
    Map,
}

/// Aggregation mode for KPI and Gauge widgets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::AggregationMode)]
#[serde(rename_all = "snake_case")]
pub enum AggregationModeDto {
    /// Use the last received value (default).
    Last,
    /// Use the first received value.
    First,
    /// Sum all values.
    Sum,
    /// Average all values.
    Avg,
    /// Count of rows.
    Count,
    /// Minimum value.
    Min,
    /// Maximum value.
    Max,
    /// Filter to a specific row by field/value match.
    Filter,
}

/// A single widget in a predefined dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::DashboardWidget)]
#[serde(rename_all = "camelCase")]
pub struct DashboardWidgetDto {
    /// Unique widget identifier.
    pub id: String,

    /// Widget type.
    #[serde(rename = "type")]
    pub widget_type: WidgetTypeDto,

    /// Display title shown in the widget header.
    pub title: String,

    /// Grid position and size.
    #[serde(default)]
    pub grid: Option<WidgetGridDto>,

    /// Widget-specific configuration (query ID, field mappings, etc.).
    #[serde(default)]
    pub config: serde_json::Value,
}

/// A predefined dashboard seeded on startup.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::PredefinedDashboard)]
#[serde(rename_all = "camelCase")]
pub struct PredefinedDashboardDto {
    /// Stable dashboard ID (prevents duplicates on restart).
    pub id: String,

    /// Dashboard display name.
    pub name: String,

    /// Grid layout options.
    #[serde(default)]
    pub grid_options: Option<GridOptionsDto>,

    /// Widgets to include in the dashboard.
    #[serde(default)]
    pub widgets: Vec<DashboardWidgetDto>,
}

// ---------------------------------------------------------------------------
// Main config DTO
// ---------------------------------------------------------------------------

/// Dashboard reaction config DTO.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::dashboard::DashboardReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct DashboardReactionConfigDto {
    /// Host to bind dashboard server.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub host: Option<ConfigValue<String>>,

    /// Port to bind dashboard server.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub port: Option<ConfigValue<u16>>,

    /// WebSocket heartbeat interval in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub heartbeat_interval_ms: Option<ConfigValue<u64>>,

    /// Optional base URL for the DrasiLib results API (e.g., "http://localhost:8080").
    /// When set, the dashboard proxies initial query data from this API
    /// so widgets populate immediately with bootstrap data.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub results_api_url: Option<ConfigValue<String>>,

    /// Optional priority queue capacity for change event processing.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,

    /// Predefined dashboards seeded on startup. Only seeded if a dashboard
    /// with the same ID does not already exist, so user edits are preserved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predefined_dashboards: Vec<PredefinedDashboardDto>,
}

// ---------------------------------------------------------------------------
// DTO → domain model conversion helpers
// ---------------------------------------------------------------------------

pub(crate) fn map_grid_options(dto: &GridOptionsDto) -> GridOptions {
    GridOptions {
        columns: dto.columns,
        row_height: dto.row_height,
        margin: dto.margin,
    }
}

pub(crate) fn map_widget_grid(dto: &WidgetGridDto) -> WidgetGrid {
    WidgetGrid {
        x: dto.x,
        y: dto.y,
        w: dto.w,
        h: dto.h,
    }
}

pub(crate) fn map_widget(dto: &DashboardWidgetDto) -> DashboardWidget {
    // Serialize the enum to its snake_case string (e.g. WidgetTypeDto::BarChart → "bar_chart")
    let widget_type_str = serde_json::to_value(&dto.widget_type)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    DashboardWidget {
        id: dto.id.clone(),
        widget_type: widget_type_str,
        title: dto.title.clone(),
        grid: dto.grid.as_ref().map(map_widget_grid).unwrap_or_default(),
        config: dto.config.clone(),
    }
}

pub(crate) fn map_predefined_dashboard(dto: &PredefinedDashboardDto) -> DashboardConfig {
    let now = Utc::now();
    DashboardConfig {
        id: dto.id.clone(),
        name: dto.name.clone(),
        created_at: now,
        updated_at: now,
        grid_options: dto
            .grid_options
            .as_ref()
            .map(map_grid_options)
            .unwrap_or_default(),
        widgets: dto.widgets.iter().map(map_widget).collect(),
    }
}

// ---------------------------------------------------------------------------
// Domain model → DTO conversion (for properties() fallback on builder path)
// ---------------------------------------------------------------------------

impl From<&crate::config::DashboardReactionConfig> for DashboardReactionConfigDto {
    fn from(config: &crate::config::DashboardReactionConfig) -> Self {
        Self {
            host: Some(ConfigValue::Static(config.host.clone())),
            port: Some(ConfigValue::Static(config.port)),
            heartbeat_interval_ms: Some(ConfigValue::Static(config.heartbeat_interval_ms)),
            results_api_url: config
                .results_api_url
                .as_ref()
                .map(|u| ConfigValue::Static(u.clone())),
            priority_queue_capacity: None,
            predefined_dashboards: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI schema registration
// ---------------------------------------------------------------------------

#[derive(OpenApi)]
#[openapi(components(schemas(
    DashboardReactionConfigDto,
    GridOptionsDto,
    WidgetGridDto,
    DashboardWidgetDto,
    PredefinedDashboardDto,
    WidgetTypeDto,
    AggregationModeDto,
)))]
struct DashboardReactionSchemas;

// ---------------------------------------------------------------------------
// Plugin descriptor
// ---------------------------------------------------------------------------

/// Descriptor for creating dashboard reaction instances.
pub struct DashboardReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for DashboardReactionDescriptor {
    fn kind(&self) -> &str {
        "dashboard"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.dashboard.DashboardReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = DashboardReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: DashboardReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = DashboardReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref host) = dto.host {
            builder = builder.with_host(mapper.resolve_string(host).await?);
        }
        if let Some(ref port) = dto.port {
            builder = builder.with_port(mapper.resolve_typed(port).await?);
        }
        if let Some(ref heartbeat_interval_ms) = dto.heartbeat_interval_ms {
            builder = builder
                .with_heartbeat_interval_ms(mapper.resolve_typed(heartbeat_interval_ms).await?);
        }
        if let Some(ref results_api_url) = dto.results_api_url {
            builder = builder.with_results_api_url(mapper.resolve_string(results_api_url).await?);
        }
        if let Some(ref priority_queue_capacity) = dto.priority_queue_capacity {
            let capacity: u64 = mapper.resolve_typed(priority_queue_capacity).await?;
            builder = builder.with_priority_queue_capacity(capacity as usize);
        }
        for dashboard_dto in &dto.predefined_dashboards {
            builder = builder.with_dashboard(map_predefined_dashboard(dashboard_dto));
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}
