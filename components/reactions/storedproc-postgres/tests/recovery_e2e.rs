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

//! Recovery E2E tests for the PostgreSQL Stored Procedure Reaction.
//!
//! These tests provision their own throwaway PostgreSQL instance via
//! testcontainers (like the sibling `postgres_storedproc_tests` suite), create
//! the stored procedures the recovery scenario exercises, and run self-contained
//! with no manual setup.

mod postgres_helpers;

use anyhow::Result;
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
use postgres_helpers::{setup_postgres, PostgresConfig};
use serial_test::serial;
use shared_tests::recovery_test_helpers::exercise_strict_gap_failure;

fn postgres_default_template() -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL handle_person_add({{param after.name}})",
        )),
        updated: None,
        deleted: None,
    }
}

/// Create the stored procedure(s) the recovery scenario invokes.
async fn setup_recovery_procedures(config: &PostgresConfig) -> Result<()> {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    // Log table so procedure calls have an observable side effect.
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS person_log (
                id SERIAL PRIMARY KEY,
                name TEXT NOT NULL,
                logged_at TIMESTAMPTZ DEFAULT NOW()
            )",
            &[],
        )
        .await?;

    // Procedure invoked by the reaction's `added` template.
    client
        .execute(
            "CREATE OR REPLACE PROCEDURE handle_person_add(p_name TEXT)
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO person_log (name) VALUES (p_name);
            END;
            $$",
            &[],
        )
        .await?;

    Ok(())
}

#[tokio::test]
#[serial]
async fn test_storedproc_postgres_strict_recovery() -> Result<()> {
    let pg = setup_postgres().await;
    let pg_config = pg.config().clone();
    setup_recovery_procedures(&pg_config).await?;

    let reaction = PostgresStoredProcReaction::builder("pg-strict")
        .with_connection(
            &pg_config.host,
            pg_config.port,
            &pg_config.database,
            &pg_config.user,
            &pg_config.password,
        )
        .with_query("q1")
        .with_default_template(postgres_default_template())
        .build()
        .await?;

    let result =
        exercise_strict_gap_failure("storedproc-postgres-strict", "pg-strict", reaction).await;

    pg.cleanup().await;
    result
}
