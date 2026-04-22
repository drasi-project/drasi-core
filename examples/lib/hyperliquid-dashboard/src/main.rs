use anyhow::Result;
use axum::{response::Html, routing::get, Router};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_sse::SseReaction;
use drasi_source_hyperliquid::{HyperliquidNetwork, HyperliquidSource};
use std::sync::Arc;

const DASHBOARD_HTML: &str = include_str!("../static/index.html");

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let source = HyperliquidSource::builder("hl")
        .with_network(HyperliquidNetwork::Mainnet)
        .with_coins(vec!["BTC", "ETH", "SOL"])
        .with_trades(true)
        .with_mid_prices(true)
        .with_order_book(true)
        .with_funding_rates(true)
        .with_liquidations(true)
        .build()?;

    let prices = Query::cypher("prices")
        .query(
            r#"
            MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
            RETURN c.name AS coin, m.price AS price, m.timestamp AS ts
            "#,
        )
        .from_source("hl")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let trades = Query::cypher("trades")
        .query(
            r#"
            MATCH (t:Trade)-[:TRADED_ON]->(c:Coin)
            RETURN c.name AS coin, t.side AS side, t.price AS price,
                   t.size AS size, t.timestamp AS ts
            "#,
        )
        .from_source("hl")
        .auto_start(true)
        .build();

    let funding = Query::cypher("funding")
        .query(
            r#"
            MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
            RETURN c.name AS coin, f.rate AS rate,
                   f.premium AS premium, f.timestamp AS ts
            "#,
        )
        .from_source("hl")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let liquidations = Query::cypher("liquidations")
        .query(
            r#"
            MATCH (l:Liquidation)-[:LIQUIDATED_ON]->(c:Coin)
            RETURN c.name AS coin, l.side AS side, l.price AS price,
                   l.size AS size, l.timestamp AS ts
            "#,
        )
        .from_source("hl")
        .auto_start(true)
        .build();

    let books = Query::cypher("books")
        .query(
            r#"
            MATCH (o:OrderBook)-[:BOOK_OF]->(c:Coin)
            RETURN c.name AS coin, o.best_bid_price AS bid, o.best_ask_price AS ask,
                   o.best_bid_size AS bid_size, o.best_ask_size AS ask_size,
                   o.bid_depth AS bid_depth, o.ask_depth AS ask_depth, o.timestamp AS ts
            "#,
        )
        .from_source("hl")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let sse = SseReaction::builder("dashboard-sse")
        .with_queries(vec![
            "prices".to_string(),
            "trades".to_string(),
            "funding".to_string(),
            "liquidations".to_string(),
            "books".to_string(),
        ])
        .with_port(8080)
        .build()?;

    // Serve dashboard UI
    tokio::spawn(async {
        let app = Router::new().route("/", get(|| async { Html(DASHBOARD_HTML) }));
        let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
        log::info!("Dashboard UI: http://localhost:3000");
        axum::serve(listener, app).await.unwrap();
    });

    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(prices)
            .with_query(trades)
            .with_query(funding)
            .with_query(liquidations)
            .with_query(books)
            .with_reaction(sse)
            .build()
            .await?,
    );

    core.start().await?;
    println!("\n  📊 Hyperliquid Dashboard");
    println!("  ────────────────────────");
    println!("  UI:  http://localhost:3000");
    println!("  SSE: http://localhost:8080/events");
    println!("\n  Press Ctrl+C to stop\n");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;

    Ok(())
}
