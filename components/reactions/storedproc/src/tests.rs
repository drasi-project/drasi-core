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

#[cfg(test)]
mod tests {
    use crate::{DatabaseClient, StoredProcReaction};
    use drasi_lib::plugin_core::Reaction;

    // Note: These tests will fail if PostgreSQL is not available
    // They validate the configuration and builder patterns, but skip actual DB connection

    #[tokio::test]
    #[ignore] // Ignore by default since it requires PostgreSQL
    async fn test_builder_basic() {
        let reaction = StoredProcReaction::builder("test-reaction")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_connection("localhost", 5432, "testdb", "user", "pass")
            .with_query("test-query")
            .with_added_command("CALL add_item(@id, @name)")
            .build()
            .await;

        assert!(reaction.is_ok());
        let reaction = reaction.unwrap();
        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.type_name(), "storedproc");
    }

    #[tokio::test]
    #[ignore] // Ignore by default since it requires PostgreSQL
    async fn test_builder_validation_no_commands() {
        let reaction = StoredProcReaction::builder("test-reaction")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_connection("localhost", 5432, "testdb", "user", "pass")
            .with_query("test-query")
            .build()
            .await;

        assert!(reaction.is_err());
        assert!(reaction
            .unwrap_err()
            .to_string()
            .contains("At least one command"));
    }

    #[tokio::test]
    async fn test_builder_validation_no_user() {
        let reaction = StoredProcReaction::builder("test-reaction")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_database("testdb")
            .with_query("test-query")
            .with_added_command("CALL test()")
            .build()
            .await;

        // This should fail during validation before trying to connect
        assert!(reaction.is_err());
        assert!(reaction.unwrap_err().to_string().contains("user is required"));
    }

    #[tokio::test]
    #[ignore] // Ignore by default since it requires PostgreSQL
    async fn test_builder_all_commands() {
        let reaction = StoredProcReaction::builder("test-reaction")
            .with_connection("localhost", 5432, "testdb", "user", "pass")
            .with_query("test-query")
            .with_added_command("CALL add(@id)")
            .with_updated_command("CALL update(@id)")
            .with_deleted_command("CALL delete(@id)")
            .with_connection_pool_size(20)
            .with_command_timeout_ms(60000)
            .with_retry_attempts(5)
            .build()
            .await;

        assert!(reaction.is_ok());
    }
}
