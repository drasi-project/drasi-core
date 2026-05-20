use anyhow::Result;
use drasi_bootstrap_oracle::OracleBootstrapProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::LogReaction;
use drasi_source_oracle::{OracleSource, StartPosition};

#[tokio::main]
async fn main() -> Result<()> {
    let host = std::env::var("ORACLE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("ORACLE_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(1522);
    let service = std::env::var("ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".to_string());
    let user = std::env::var("ORACLE_USER").unwrap_or_else(|_| "system".to_string());
    let password = std::env::var("ORACLE_PASSWORD").unwrap_or_else(|_| "drasi123".to_string());

    let bootstrap_provider = OracleBootstrapProvider::builder()
        .with_host(&host)
        .with_port(port)
        .with_service(&service)
        .with_user(&user)
        .with_password(&password)
        .with_table("SYSTEM.DRASI_PRODUCTS")
        .build()?;

    let source = OracleSource::builder("oracle-source")
        .with_host(&host)
        .with_port(port)
        .with_service(&service)
        .with_user(&user)
        .with_password(&password)
        .with_table("SYSTEM.DRASI_PRODUCTS")
        .with_poll_interval_ms(1000)
        .with_start_position(StartPosition::Current)
        .with_bootstrap_provider(bootstrap_provider)
        .build()?;

    let query = Query::cypher("oracle-products-query")
        .query(
            r#"
            MATCH (p:drasi_products)
            RETURN p.id AS id, p.name AS name, p.price AS price
        "#,
        )
        .from_source("oracle-source")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let reaction = LogReaction::builder("oracle-log-reaction")
        .with_query("oracle-products-query")
        .build()?;

    let drasi = DrasiLib::builder()
        .with_id("oracle-getting-started")
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;

    println!("Oracle example is running.");
    println!("Use ./test-updates.sh in this directory to generate INSERT/UPDATE/DELETE changes.");
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    drasi.stop().await?;
    Ok(())
}
