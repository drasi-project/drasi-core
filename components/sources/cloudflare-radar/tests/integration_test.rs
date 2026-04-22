use anyhow::Result;
use drasi_bootstrap_cloudflare_radar::CloudflareRadarBootstrapProvider;
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_cloudflare_radar::{CloudflareRadarSource, StartBehavior};
use serde_json::Value;
use std::time::Duration;
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const SOURCE_ID: &str = "radar-source";
const QUERY_ID: &str = "hijack-query";

fn value_as_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn field_matches(data: &Value, field: &str, expected: &str) -> bool {
    data.get(field)
        .and_then(value_as_string)
        .map(|value| value == expected)
        .unwrap_or(false)
}

fn matches_fields(data: &Value, fields: &[(&str, &str)]) -> bool {
    fields
        .iter()
        .all(|(field, expected)| field_matches(data, field, expected))
}

fn matches_change(entry: &ResultDiff, change_type: &str, fields: &[(&str, &str)]) -> bool {
    match (change_type, entry) {
        ("ADD", ResultDiff::Add { data })
        | ("DELETE", ResultDiff::Delete { data })
        | ("UPDATE", ResultDiff::Update { data, .. }) => matches_fields(data, fields),
        _ => false,
    }
}

fn matches_update(
    entry: &ResultDiff,
    before_fields: &[(&str, &str)],
    after_fields: &[(&str, &str)],
) -> bool {
    match entry {
        ResultDiff::Update { before, after, .. } => {
            matches_fields(before, before_fields) && matches_fields(after, after_fields)
        }
        _ => false,
    }
}

async fn wait_for_change<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    attempts: usize,
    mut matcher: F,
) -> Result<ResultDiff>
where
    F: FnMut(&ResultDiff) -> bool,
{
    for _ in 0..attempts {
        if let Some(result) = subscription.recv().await {
            for entry in &result.results {
                if matcher(entry) {
                    return Ok(entry.clone());
                }
            }
        }
    }

    anyhow::bail!("Timed out waiting for expected change event");
}

async fn setup_wiremock() -> Result<(ContainerAsync<GenericImage>, String)> {
    let image = GenericImage::new("wiremock/wiremock", "3.3.1").with_exposed_port(8080.tcp());
    let container = image.start().await?;
    let port = container.get_host_port_ipv4(8080.tcp()).await?;
    let host = container.get_host().await?.to_string();
    let admin_base = format!("http://{host}:{port}");
    wait_for_wiremock_ready(&admin_base).await?;
    Ok((container, admin_base))
}

async fn wait_for_wiremock_ready(admin_base: &str) -> Result<()> {
    let client = reqwest::Client::new();
    for _ in 0..30 {
        let response = client.get(format!("{admin_base}/__admin")).send().await;
        if response
            .as_ref()
            .ok()
            .map(|res| res.status().is_success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("Timed out waiting for Wiremock to become ready");
}

async fn reset_mappings(admin_base: &str) -> Result<()> {
    let client = reqwest::Client::new();
    client
        .delete(format!("{admin_base}/__admin/mappings"))
        .send()
        .await?;
    Ok(())
}

async fn stub_hijack_response(admin_base: &str, response: Value) -> Result<()> {
    reset_mappings(admin_base).await?;
    let mapping = serde_json::json!({
        "request": {
            "method": "GET",
            "urlPath": "/client/v4/radar/bgp/hijacks/events"
        },
        "response": {
            "status": 200,
            "jsonBody": response
        }
    });
    let client = reqwest::Client::new();
    client
        .post(format!("{admin_base}/__admin/mappings"))
        .json(&mapping)
        .send()
        .await?;
    Ok(())
}

fn hijack_response(confidence: u32) -> Value {
    serde_json::json!({
        "success": true,
        "errors": [],
        "result": {
            "events": [{
                "id": 1234,
                "duration": 0,
                "event_type": 0,
                "hijack_msgs_count": 1,
                "hijacker_asn": 64512,
                "is_stale": false,
                "max_hijack_ts": "2023-04-27T14:01:55.952Z",
                "min_hijack_ts": "2023-04-27T14:01:55.952Z",
                "peer_asns": [8455],
                "peer_ip_count": 1,
                "prefixes": ["192.0.2.0/24"],
                "victim_asns": [64513],
                "confidence_score": confidence
            }]
        }
    })
}

fn empty_hijack_response() -> Value {
    serde_json::json!({
        "success": true,
        "errors": [],
        "result": {
            "events": []
        }
    })
}

#[tokio::test]
#[ignore]
async fn test_cloudflare_radar_hijack_changes() -> Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let (container, admin_base) = setup_wiremock().await?;
    let api_base_url = format!("{admin_base}/client/v4");

    stub_hijack_response(&admin_base, empty_hijack_response()).await?;

    let bootstrap_provider = CloudflareRadarBootstrapProvider::builder()
        .with_api_token("test-token")
        .with_api_base_url(&api_base_url)
        .with_category("bgp_hijacks", true)
        .build()?;

    let source = CloudflareRadarSource::builder(SOURCE_ID)
        .with_api_token("test-token")
        .with_api_base_url(&api_base_url)
        .with_poll_interval_secs(1)
        .with_category("bgp_hijacks", true)
        .with_start_behavior(StartBehavior::StartFromBeginning)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher(QUERY_ID)
        .query(
            r#"
            MATCH (h:BgpHijack)
            RETURN h.eventId AS eventId, h.confidenceScore AS confidence
        "#,
        )
        .from_source(SOURCE_ID)
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let (reaction, handle) = ApplicationReaction::builder("app-reaction")
        .with_query(QUERY_ID)
        .build();

    let core = DrasiLib::builder()
        .with_id("radar-integration-test")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    core.start().await?;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    stub_hijack_response(&admin_base, hijack_response(4)).await?;
    wait_for_change(&mut subscription, 6, |entry| {
        matches_change(entry, "ADD", &[("eventId", "1234"), ("confidence", "4")])
    })
    .await?;

    stub_hijack_response(&admin_base, hijack_response(9)).await?;
    wait_for_change(&mut subscription, 6, |entry| {
        matches_update(
            entry,
            &[("eventId", "1234"), ("confidence", "4")],
            &[("eventId", "1234"), ("confidence", "9")],
        )
    })
    .await?;

    stub_hijack_response(&admin_base, empty_hijack_response()).await?;
    wait_for_change(&mut subscription, 6, |entry| {
        matches_change(entry, "DELETE", &[("eventId", "1234")])
    })
    .await?;

    core.stop().await?;
    drop(container);
    Ok(())
}
