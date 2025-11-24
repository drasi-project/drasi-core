// Bidirectional Integration Example
// Demonstrates sending data via ApplicationSource and receiving via ApplicationReaction

use anyhow::Result;
use drasi_lib::sources::application::PropertyMapBuilder;
use drasi_lib::{DrasiServerCore, Query, Reaction, Source};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    // Configure server with Application source and reaction
    let core = DrasiServerCore::builder()
        .with_id("bidirectional-example")
        // Application source - we'll send data into it
        .add_source(Source::application("app-source").build())
        // Query to filter temperatures > 75
        .add_query(
            Query::cypher("high-temp-alert")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s.id, s.temperature")
                .from_source("app-source")
                .build(),
        )
        // Application reaction - we'll receive results from it
        .add_reaction(
            Reaction::application("app-reaction")
                .subscribe_to("high-temp-alert")
                .build(),
        )
        .build()
        .await?;

    // Start the server
    core.start().await?;

    // Get handles for bidirectional communication
    let source = core.source_handle("app-source").await?;
    let reaction = core.reaction_handle("app-reaction").await?;

    // Spawn task to receive results
    let receiver = tokio::spawn(async move {
        let mut stream = reaction.as_stream().await.unwrap();
        println!("Listening for query results...\n");

        while let Some(result) = stream.next().await {
            println!("Received change from query '{}':", result.query_id);
            println!("  Results: {:?}\n", result.results);
        }
    });

    // Simulate sensor data
    println!("Sending sensor data...\n");

    // Sensor reading below threshold - won't trigger query
    println!("1. Sending sensor-1 with temp=70 (below threshold)");
    source
        .send_node_insert(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_integer("temperature", 70)
                .with_string("location", "room-a")
                .build(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Sensor reading above threshold - will trigger Adding
    println!("2. Sending sensor-2 with temp=80 (above threshold)");
    source
        .send_node_insert(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_integer("temperature", 80)
                .with_string("location", "room-b")
                .build(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Update sensor-2 temperature higher - will trigger Updating
    println!("3. Updating sensor-2 temp to 85");
    source
        .send_node_update(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_integer("temperature", 85)
                .with_string("location", "room-b")
                .build(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Update sensor-1 to cross threshold - will trigger Adding
    println!("4. Updating sensor-1 temp to 78 (crosses threshold)");
    source
        .send_node_update(
            "sensor-1",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-1")
                .with_integer("temperature", 78)
                .with_string("location", "room-a")
                .build(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Update sensor-2 to drop below threshold - will trigger Removing
    println!("5. Updating sensor-2 temp to 70 (drops below threshold)");
    source
        .send_node_update(
            "sensor-2",
            vec!["Sensor"],
            PropertyMapBuilder::new()
                .with_string("id", "sensor-2")
                .with_integer("temperature", 70)
                .with_string("location", "room-b")
                .build(),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Delete sensor-1 - will trigger Removing
    println!("6. Deleting sensor-1");
    source.send_delete("sensor-1", vec!["Sensor"]).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    println!("\nExample complete! Shutting down...");

    // Stop the server
    core.stop().await?;

    // Clean shutdown of receiver
    drop(receiver);

    Ok(())
}
