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

use anyhow::Result;
use drasi_reaction_storedproc_mysql::{MySqlStoredProcReaction, QueryConfig, TemplateSpec};
use shared_tests::recovery_test_helpers::exercise_strict_gap_failure;

fn mysql_default_template() -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL handle_person_add({{param after.name}})",
        )),
        updated: None,
        deleted: None,
    }
}

#[tokio::test]
#[ignore = "requires MySQL connection"]
async fn test_storedproc_mysql_strict_recovery() -> Result<()> {
    let reaction = MySqlStoredProcReaction::builder("mysql-strict")
        .with_connection("localhost", 3306, "test", "test", "test")
        .with_query("q1")
        .with_default_template(mysql_default_template())
        .build()
        .await?;

    exercise_strict_gap_failure("storedproc-mysql-strict", "mysql-strict", reaction).await
}
