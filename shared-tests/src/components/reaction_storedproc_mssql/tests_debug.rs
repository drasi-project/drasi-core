// Debug test without serial attribute
use crate::mssql_helpers::setup_mssql;
use std::time::Duration;

#[tokio::test]
async fn test_mssql_connection_no_serial() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    log::info!("=== TEST STARTED (NO SERIAL) ===");
    println!("=== TEST STARTED (NO SERIAL) ===");

    let result = tokio::time::timeout(
        Duration::from_secs(120),
        async {
            log::info!("Calling setup_mssql()...");
            let mssql = setup_mssql().await;
            log::info!("setup_mssql() completed");

            let mut client = mssql.get_client().await.unwrap();
            let rows = client
                .query("SELECT 1 AS value", &[])
                .await
                .unwrap()
                .into_results()
                .await
                .unwrap();

            if let Some(rows) = rows.first() {
                if let Some(row) = rows.first() {
                    let value: i32 = row.get(0).unwrap();
                    assert_eq!(value, 1);
                }
            }

            mssql.cleanup().await;
        }
    ).await;

    match result {
        Ok(_) => {
            log::info!("Test completed successfully");
        },
        Err(_) => panic!("Test timed out after 120 seconds"),
    }
}
