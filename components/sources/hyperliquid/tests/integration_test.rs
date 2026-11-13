use anyhow::Result;
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::{subscription::SubscriptionOptions, ApplicationReaction};
use drasi_source_hyperliquid::{
    HyperliquidBootstrapProvider, HyperliquidNetwork, HyperliquidSource,
};
use std::time::{Duration, Instant};

const SOURCE_ID: &str = "hl-test";
const PRICE_QUERY: &str = "price-query";
const TRADE_QUERY: &str = "latest-trade";

#[tokio::test]
#[ignore]
async fn test_hyperliquid_source_live() -> Result<()> {
    let bootstrap_config = drasi_source_hyperliquid::HyperliquidSourceConfig::default();
    let bootstrap = HyperliquidBootstrapProvider::new(bootstrap_config.clone());

    let source = HyperliquidSource::builder(
        drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000325")
            .expect("fixture UUID"),
    )
    .with_network(HyperliquidNetwork::Mainnet)
    .with_coins(vec!["BTC"])
    .with_trades(true)
    .with_order_book(true)
    .with_mid_prices(true)
    .with_liquidations(true)
    .with_funding_rates(false)
    .start_from_beginning()
    .with_bootstrap_provider(bootstrap)
    .build()?;

    let price_query = Query::cypher(
        drasi_core::ComponentName::try_new("price-query").expect("fixture component name"),
    )
    .query(
        r#"
            MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
            WHERE c.name = 'BTC'
            RETURN c.name AS coin, c.max_leverage AS max_leverage, m.price AS price
            "#,
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("hl-test").expect("fixture component name"),
    )
    .auto_start(true)
    .enable_bootstrap(true)
    .build()?;

    let trade_query = Query::cypher(
        drasi_core::ComponentName::try_new("latest-trade").expect("fixture component name"),
    )
    .query(
        r#"
            MATCH (t:Trade)
            WHERE t.coin = 'BTC'
            RETURN t.tid AS tid, t.price AS price
            "#,
    )
    .from_input_by_name(
        drasi_core::ComponentName::try_new("hl-test").expect("fixture component name"),
    )
    .auto_start(true)
    .enable_bootstrap(false)
    .build()?;

    let (reaction, handle) = ApplicationReaction::builder(
        drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000001")
            .expect("fixture UUID"),
    )
    .with_query_by_name(
        drasi_core::ComponentName::try_new("price-query").expect("fixture component name"),
    )
    .with_query_by_name(
        drasi_core::ComponentName::try_new("latest-trade").expect("fixture component name"),
    )
    .build();

    let core = DrasiLib::builder()
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("hl-integration-test")
        .with_source(
            drasi_core::ComponentName::try_new("hl-test").expect("fixture component name"),
            source,
        )
        .with_query(price_query)
        .with_query(trade_query)
        .with_reaction(
            drasi_core::ComponentName::try_new("app-reaction").expect("fixture component name"),
            reaction,
        )
        .build()
        .await?;

    core.start().await?;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(2)))
        .await?;

    let mut saw_price_add = false;
    let mut saw_price_update = false;
    let mut trade_event_count = 0usize;
    let mut saw_trade_change = false;

    let start = Instant::now();
    let timeout = Duration::from_secs(60);

    while start.elapsed() < timeout {
        if let Some(result) = subscription.recv().await {
            if result.query_id
                == drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000382")
                    .expect("fixture UUID")
            {
                for entry in &result.results {
                    match entry {
                        ResultDiff::Add { data, .. } if data.get("coin").is_some() => {
                            saw_price_add = true;
                        }
                        ResultDiff::Update { .. } => {
                            if saw_price_add {
                                saw_price_update = true;
                            } else {
                                saw_price_add = true;
                                saw_price_update = true;
                            }
                        }
                        _ => {}
                    }
                }
            }

            if result.query_id
                == drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000383")
                    .expect("fixture UUID")
            {
                for entry in &result.results {
                    if matches!(
                        entry,
                        ResultDiff::Add { .. }
                            | ResultDiff::Update { .. }
                            | ResultDiff::Aggregation { .. }
                            | ResultDiff::Delete { .. }
                    ) {
                        trade_event_count += 1;
                        if trade_event_count > 1 {
                            saw_trade_change = true;
                        }
                    }
                }
            }
        }

        if saw_price_add && saw_price_update && trade_event_count > 0 && saw_trade_change {
            break;
        }
    }

    if !saw_price_add {
        anyhow::bail!("Did not observe initial MidPrice add event");
    }
    if !saw_price_update {
        anyhow::bail!("Did not observe MidPrice update event");
    }
    if trade_event_count == 0 {
        anyhow::bail!("Did not observe trade events");
    }
    if !saw_trade_change {
        anyhow::bail!("Did not observe trade change events");
    }

    core.stop().await?;
    Ok(())
}
