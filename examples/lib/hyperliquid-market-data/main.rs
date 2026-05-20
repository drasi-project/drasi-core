use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::LogReaction;
use drasi_source_hyperliquid::{HyperliquidNetwork, HyperliquidSource};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let source = HyperliquidSource::builder("hl-source")
        .with_network(HyperliquidNetwork::Mainnet)
        .with_coins(vec!["BTC", "ETH"])
        .with_trades(true)
        .with_mid_prices(true)
        .with_order_book(true)
        .with_funding_rates(true)
        .with_liquidations(true)
        .build()?;

    let price_query = Query::cypher("btc-price-alerts")
        .query(
            r#"
            MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
            WHERE c.name = 'BTC'
            RETURN c.name AS coin, c.max_leverage AS max_leverage, m.price AS price
            "#,
        )
        .from_source("hl-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let trade_query = Query::cypher("large-trades")
        .query(
            r#"
            MATCH (t:Trade)-[:TRADED_ON]->(c:Coin)
            WHERE t.size > 1.0
            RETURN c.name AS coin, c.max_leverage AS max_leverage, t.side AS side,
                   t.price AS price, t.size AS size
            "#,
        )
        .from_source("hl-source")
        .auto_start(true)
        .build();

    let log_reaction = LogReaction::builder("console")
        .from_query("btc-price-alerts")
        .from_query("large-trades")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(price_query)
            .with_query(trade_query)
            .with_reaction(log_reaction)
            .build()
            .await?,
    );

    core.start().await?;
    println!("Streaming Hyperliquid data... Press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;

    Ok(())
}
