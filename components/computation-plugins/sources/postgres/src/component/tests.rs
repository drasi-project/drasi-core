// Copyright 2026 The Drasi Authors.
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

use super::*;
use crate::rows::RowEmission;
use bytes::Bytes;
use drasi_computation_contracts::{SourceProgressKey, SourceTransactionLimits};
use drasi_postgres_common::{
    config::{PostgresSourceConfig, SslMode},
    connection::ReplicationConnection,
    replication::{Position, ReplicationIo},
};
use std::{
    collections::BTreeMap,
    num::{NonZeroU64, NonZeroUsize},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn begin() -> Vec<u8> {
    let mut bytes = vec![b'B'];
    bytes.extend(100u64.to_be_bytes());
    bytes.extend(1_000_000i64.to_be_bytes());
    bytes.extend(5u32.to_be_bytes());
    bytes
}

fn commit() -> Vec<u8> {
    let mut bytes = vec![b'C', 0];
    bytes.extend(100u64.to_be_bytes());
    bytes.extend(120u64.to_be_bytes());
    bytes.extend(1_000_000i64.to_be_bytes());
    bytes
}

fn relation() -> Vec<u8> {
    let mut bytes = vec![b'R'];
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(b"public\0person\0");
    bytes.push(b'd');
    bytes.extend(2u16.to_be_bytes());
    for (name, oid, key) in [("id", 23u32, true), ("name", 25, false)] {
        bytes.push(u8::from(key));
        bytes.extend(name.as_bytes());
        bytes.push(0);
        bytes.extend(oid.to_be_bytes());
        bytes.extend((-1i32).to_be_bytes());
    }
    bytes
}

fn insert(id: &[u8], name: &[u8]) -> Vec<u8> {
    let mut bytes = vec![b'I'];
    bytes.extend(1u32.to_be_bytes());
    bytes.push(b'N');
    bytes.extend(2u16.to_be_bytes());
    for value in [id, name] {
        bytes.push(b't');
        bytes.extend(u32::try_from(value.len()).unwrap().to_be_bytes());
        bytes.extend(value);
    }
    bytes
}

fn strict() -> PgOutputDecoder {
    PgOutputDecoder::strict(NonZeroUsize::new(65536).unwrap())
}

fn transactions(limits: SourceTransactionLimits) -> Transactions {
    Transactions {
        decoder: strict(),
        pending: None,
        keys: BTreeMap::from([(("public".into(), "person".into()), vec!["id".into()])]),
        source: ComponentId::try_new("source").unwrap(),
        stream: StreamId::try_new("stream").unwrap(),
        limits,
        binding: [1; 32],
        managed_slot: None,
        resume: 0,
        transport_sequence: Arc::new(AtomicU64::new(0)),
        filtered: false,
    }
}

#[test]
fn strict_decoder_rejects_invalid_boundaries_and_cannot_resume_after_failure() {
    for invalid in [
        vec![],
        vec![b'?'],
        vec![b'T'],
        commit(),
        insert(b"1", b"outside"),
        [begin(), vec![0]].concat(),
        begin()[..10].to_vec(),
    ] {
        let mut decoder = strict();
        assert!(decoder.decode_message(&invalid).is_err(), "{invalid:?}");
        assert!(decoder
            .decode_message(&begin())
            .unwrap_err()
            .to_string()
            .contains("reconstruction"));
    }
    let mut decoder = strict();
    decoder.decode_message(&begin()).unwrap();
    assert!(decoder.decode_message(&begin()).is_err());
    assert!(PgOutputDecoder::new()
        .decode_message(b"?")
        .unwrap()
        .is_none());
}

#[test]
fn strict_decoder_validates_commit_identity_flags_and_timestamp_overflow() {
    for index in [1, 9, 25] {
        let mut decoder = strict();
        decoder.decode_message(&begin()).unwrap();
        let mut invalid = commit();
        invalid[index] ^= 1;
        assert!(decoder.decode_message(&invalid).is_err());
    }
    let mut decoder = strict();
    decoder.decode_message(&begin()).unwrap();
    let mut invalid = commit();
    invalid[10..18].fill(0);
    assert!(decoder.decode_message(&invalid).is_err());
    let mut bytes = begin();
    bytes[9..17].copy_from_slice(&i64::MAX.to_be_bytes());
    assert!(strict().decode_message(&bytes).is_err());
}

#[test]
fn strict_decoder_rejects_bad_values_and_trailing_or_missing_tuple_columns() {
    for invalid in [
        insert(b"not-an-integer", b"name"),
        insert(b"1", &[255]),
        [insert(b"1", b"name"), vec![0]].concat(),
    ] {
        let mut decoder = strict();
        decoder.decode_message(&begin()).unwrap();
        decoder.decode_message(&relation()).unwrap();
        assert!(decoder.decode_message(&invalid).is_err());
    }
    let mut decoder = strict();
    decoder.decode_message(&begin()).unwrap();
    decoder.decode_message(&relation()).unwrap();
    let mut invalid = insert(b"1", b"name");
    invalid[7] = 1;
    assert!(decoder.decode_message(&invalid).is_err());
}

#[test]
fn strict_decoder_bounds_relation_metadata_and_replaces_without_leaking_budget() {
    let mut small = PgOutputDecoder::strict(NonZeroUsize::new(1).unwrap());
    assert!(small.decode_message(&relation()).is_err());
    let mut decoder = strict();
    for _ in 0..1000 {
        decoder.decode_message(&relation()).unwrap();
    }
    let mut invalid = relation();
    invalid[5] = 255;
    assert!(strict().decode_message(&invalid).is_err());
}

#[tokio::test]
async fn whole_transaction_mapping_is_stable_and_never_publishes_a_prefix() {
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(1).unwrap(),
        max_bytes: NonZeroUsize::new(4096).unwrap(),
        max_duration_ms: NonZeroU64::new(1000).unwrap(),
    };
    let make = || transactions(limits);
    let mut payloads = Vec::new();
    for _ in 0..2 {
        let mut transactions = make();
        for frame in [begin(), relation(), insert(b"1", b"final")] {
            assert!(transactions.decode(&frame).unwrap().is_none());
        }
        let output = transactions.decode(&commit()).unwrap().unwrap();
        let transaction = SourceTransactionCodec::decode(&output.envelope, limits).unwrap();
        let changes = transaction.into_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].get_transaction_time(), 946_684_801_000);
        payloads.push(serde_json::to_vec(&changes).unwrap());
    }
    assert_eq!(payloads[0], payloads[1]);
    let mut transactions = make();
    for frame in [begin(), relation(), insert(b"1", b"first")] {
        transactions.decode(&frame).unwrap();
    }
    let error = transactions.decode(&insert(b"2", b"second")).unwrap_err();
    assert!(error.downcast_ref::<SourceTransactionError>().is_some());
    assert!(
        transactions.decode(&commit()).is_err(),
        "rejected prefix must never publish"
    );
}

#[test]
fn transaction_keys_preserve_tuple_boundaries_and_namespace() {
    let encode = drasi_postgres_common::transaction_element_id;
    let first = vec![("a".into(), "a_b".into()), ("b".into(), "c".into())];
    let second = vec![("a".into(), "a".into()), ("b".into(), "b_c".into())];
    assert_ne!(
        encode("public", "t", &first).unwrap(),
        encode("public", "t", &second).unwrap()
    );
    assert_ne!(
        encode("x", "y.z", &first).unwrap(),
        encode("x.y", "z", &first).unwrap()
    );
    assert!(encode("public", "t", &[]).is_err());
    let reversed = first.iter().rev().cloned().collect::<Vec<_>>();
    assert_eq!(
        encode("public", "t", &first).unwrap(),
        encode("public", "t", &reversed).unwrap()
    );
    assert!(encode(
        "public",
        "t",
        &[("id".into(), "1".into()), ("id".into(), "2".into())]
    )
    .is_err());
    assert!(parse_lsn("100000000/0").is_err());
    assert!(parse_lsn("0/100000000").is_err());
}

#[test]
fn native_checkpoint_rejects_missing_damaged_or_rebound_positions() {
    use drasi_core_types::interface::SourceCheckpoint;

    let source = ComponentId::try_new("source").unwrap();
    let key = SourceProgressKey::Source("source".into());
    let mut snapshot = SourceProgressSnapshot::default();
    assert_eq!(checkpoint(&snapshot, &source, [1; 32]).unwrap(), None);
    let position = serde_json::to_vec(&Position {
        version: 1,
        binding: [1; 32],
        commit_lsn: 100,
        managed_slot: None,
    })
    .unwrap();
    snapshot.checkpoints.insert(
        key.clone(),
        SourceCheckpoint::new(100, Some(Bytes::from(position.clone()))),
    );
    assert_eq!(checkpoint(&snapshot, &source, [1; 32]).unwrap(), Some(100));
    assert!(checkpoint(&snapshot, &source, [2; 32]).is_err());
    for invalid in [
        SourceCheckpoint::new(100, None),
        SourceCheckpoint::new(101, Some(Bytes::from(position))),
        SourceCheckpoint::new(100, Some(Bytes::from_static(b"not a cursor"))),
        SourceCheckpoint::new(
            100,
            Some(Bytes::from(
                serde_json::to_vec(&Position {
                    version: 2,
                    binding: [1; 32],
                    commit_lsn: 100,
                    managed_slot: None,
                })
                .unwrap(),
            )),
        ),
    ] {
        snapshot.checkpoints.insert(key.clone(), invalid);
        assert!(checkpoint(&snapshot, &source, [1; 32]).is_err());
    }
}

#[test]
fn native_configuration_rejects_malformed_connection_and_limits_before_start() {
    let valid = serde_json::json!({
        "connection": {"database": "database", "user": "user"},
        "start_lsn": "0/10",
        "transactions": {"max_changes": 16, "max_bytes": 4096, "max_duration_ms": 1000},
        "max_protocol_bytes": 4096,
        "io_timeout_ms": 1000,
        "feedback_interval_ms": 100,
    });
    let config: PostgresTransactionConfig = serde_json::from_value(valid.clone()).unwrap();
    config.validate().unwrap();
    let valid = serde_json::to_value(&config).unwrap();
    for (path, value) in [
        ("/connection/host", serde_json::json!("host\0suffix")),
        (
            "/connection/database",
            serde_json::json!("database\0suffix"),
        ),
        ("/connection/user", serde_json::json!("user\0suffix")),
        (
            "/connection/password",
            serde_json::json!("password\0suffix"),
        ),
        ("/connection/database", serde_json::json!("")),
        ("/max_protocol_bytes", serde_json::json!(63)),
        ("/start_lsn", serde_json::json!("100000000/0")),
    ] {
        let mut invalid = valid.clone();
        *invalid
            .pointer_mut(path)
            .unwrap_or_else(|| panic!("missing test configuration path {path}")) = value;
        let config: PostgresTransactionConfig = serde_json::from_value(invalid).unwrap();
        assert!(config.validate().is_err(), "{path}");
    }
    for path in [
        "/transactions/max_changes",
        "/transactions/max_bytes",
        "/transactions/max_duration_ms",
        "/max_protocol_bytes",
        "/io_timeout_ms",
        "/feedback_interval_ms",
    ] {
        let mut invalid = valid.clone();
        *invalid.pointer_mut(path).unwrap() = serde_json::json!(0);
        assert!(serde_json::from_value::<PostgresTransactionConfig>(invalid).is_err());
    }
    for slot in ["UPPERCASE", "unsafe';", &"a".repeat(64)] {
        let mut config = config.clone();
        config.connection.slot_name = slot.into();
        assert!(config.validate().is_err());
    }
    for field in ["hostname", "sslMode", "password_typo"] {
        let mut invalid = valid.clone();
        invalid["connection"][field] = serde_json::json!("must not silently use a default");
        assert!(
            serde_json::from_value::<PostgresTransactionConfig>(invalid).is_err(),
            "{field}"
        );
    }
}

#[test]
fn managed_handover_requires_a_completed_owner_cursor_and_exact_slot_prefix() -> Result<()> {
    use drasi_core_types::interface::SourceCheckpoint;
    let source = ComponentId::try_new("source")?;
    let config: PostgresTransactionConfig = serde_json::from_value(serde_json::json!({
        "connection": {"database": "database", "user": "user", "slot_name": "owned"},
        "start_lsn": "0/0",
        "transactions": {"max_changes": 16, "max_bytes": 4096, "max_duration_ms": 1000},
        "max_protocol_bytes": 4096, "io_timeout_ms": 1000, "feedback_interval_ms": 100
    }))?;
    let mut progress = SourceProgressSnapshot::default();
    assert!(live_config(&config, &progress, &source).is_err());
    progress.bootstrap_complete = true;
    assert!(live_config(&config, &progress, &source).is_err());
    for slot in [
        None,
        Some("owned".into()),
        Some(format!("someone_else_{}", "a".repeat(32))),
        Some(format!("owned_{}", "A".repeat(32))),
        Some(format!("owned_{}';", "a".repeat(32))),
    ] {
        progress.checkpoints.insert(
            SourceProgressKey::Source("source".into()),
            SourceCheckpoint::new(
                100,
                Some(
                    serde_json::to_vec(&Position {
                        version: 1,
                        binding: [1; 32],
                        commit_lsn: 100,
                        managed_slot: slot,
                    })?
                    .into(),
                ),
            ),
        );
        assert!(live_config(&config, &progress, &source).is_err());
    }

    let slot = format!("owned_{}", "a".repeat(32));
    progress.checkpoints.insert(
        SourceProgressKey::Source("source".into()),
        SourceCheckpoint::new(
            100,
            Some(
                serde_json::to_vec(&Position {
                    version: 1,
                    binding: [1; 32],
                    commit_lsn: 100,
                    managed_slot: Some(slot.clone()),
                })?
                .into(),
            ),
        ),
    );
    let recovered = live_config(&config, &progress, &source)?;
    assert_eq!(recovered.connection.slot_name, slot);
    assert_eq!(parse_lsn(&recovered.start_lsn)?, 100);
    progress.bootstrap_complete = false;
    assert!(live_config(&config, &progress, &source).is_err());
    Ok(())
}

#[tokio::test]
async fn ordinary_rows_preserve_partial_replay_and_confirm_only_complete_transactions() -> Result<()>
{
    use drasi_core_types::interface::SourceCheckpoint;
    use drasi_postgres_common::replication::rows::RowPosition;
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(16).unwrap(),
        max_bytes: NonZeroUsize::new(65536).unwrap(),
        max_duration_ms: NonZeroU64::new(1000).unwrap(),
    };
    let group = |transaction: &mut Transactions| -> Result<_> {
        for frame in [
            begin(),
            relation(),
            insert(b"1", b"one"),
            insert(b"2", b"two"),
        ] {
            assert!(transaction.decode(&frame)?.is_none());
        }
        transaction.decode(&commit())?.context("committed group")
    };
    let mut transaction = transactions(limits);
    let mut rows = RowEmission::new(None, 0, Arc::new(AtomicU64::new(0)));
    let output = rows.expand(group(&mut transaction)?, &transaction)?;
    assert_eq!(output.len(), 2);
    let mut progress = SourceProgressSnapshot::default();
    let first = GraphChangeCodec::source_metadata(&output[0].envelope)?.context("row metadata")?;
    let first_position = first.source_position.context("row position")?;
    progress.checkpoints.insert(
        SourceProgressKey::Source("source".into()),
        SourceCheckpoint::new(
            first.sequence.context("sequence")?,
            Some(first_position.clone().into()),
        ),
    );
    assert_eq!(
        rows.confirmed(&progress, &transaction)?,
        Some(0),
        "a handled prefix cannot confirm the commit"
    );
    let resume: RowPosition = serde_json::from_slice(&first_position)?;
    let mut replacement = RowEmission::new(Some(resume), 0, Arc::new(AtomicU64::new(10)));
    let mut transaction = transactions(limits);
    let replay = replacement.expand(group(&mut transaction)?, &transaction)?;
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].envelope.system().sequence(), 11);
    let second =
        GraphChangeCodec::source_metadata(&replay[0].envelope)?.context("replayed row metadata")?;
    assert_eq!(second.sequence, Some(2));
    progress.checkpoints.insert(
        SourceProgressKey::Source("source".into()),
        SourceCheckpoint::new(
            2,
            Some(second.source_position.context("row position")?.into()),
        ),
    );
    assert_eq!(replacement.confirmed(&progress, &transaction)?, Some(100));
    Ok(())
}

#[tokio::test]
async fn native_worker_enforces_idle_transaction_deadline_without_publishing() -> Result<()> {
    let (client, mut server) = tokio::io::duplex(4096);
    let connect = async {
        let client: Box<dyn ReplicationIo> = Box::new(client);
        let mut connection = ReplicationConnection::from_stream(
            client,
            "database",
            "user",
            "",
            NonZeroUsize::new(4096),
        )
        .await?;
        connection
            .start_replication("slot", Some(0), HashMap::new())
            .await?;
        Result::<_>::Ok(connection)
    };
    let handshake = async {
        let length = server.read_u32().await? as usize;
        server.read_exact(&mut vec![0; length - 4]).await?;
        server
            .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
            .await?;
        assert_eq!(server.read_u8().await?, b'Q');
        let length = server.read_u32().await? as usize;
        server.read_exact(&mut vec![0; length - 4]).await?;
        server.write_all(&[b'W', 0, 0, 0, 7, 0, 0, 0]).await?;
        Result::<_>::Ok(server)
    };
    let (connection, server) = tokio::join!(connect, handshake);
    let mut server = server?;
    let limits = SourceTransactionLimits {
        max_changes: NonZeroUsize::new(16).unwrap(),
        max_bytes: NonZeroUsize::new(4096).unwrap(),
        max_duration_ms: NonZeroU64::new(25).unwrap(),
    };
    let config = PostgresTransactionConfig {
        connection: PostgresSourceConfig {
            host: "unused".into(),
            port: 5432,
            database: "database".into(),
            user: "user".into(),
            password: "".into(),
            tables: vec![],
            slot_name: "slot".into(),
            publication_name: "publication".into(),
            ssl_mode: SslMode::Disable,
            table_keys: vec![],
        },
        tls_ca_pem: None,
        start_lsn: "0/0".into(),
        transactions: limits,
        max_protocol_bytes: NonZeroUsize::new(4096).unwrap(),
        io_timeout_ms: NonZeroU64::new(1000).unwrap(),
        feedback_interval_ms: NonZeroU64::new(10).unwrap(),
    };
    let (_progress, watch) = watch::channel(Arc::new(SourceProgressSnapshot {
        ready: true,
        persistent: true,
        ..Default::default()
    }));
    let mut worker = Worker {
        connection: Some(connection?),
        transactions: transactions(limits),
        progress: Box::new(watch),
        reset_generation: 0,
        confirmed: 0,
        next_feedback: Instant::now(),
        config,
        persistent: true,
        rows: None,
        reconnect: None,
    };
    let mut data = vec![b'w'];
    data.extend([0; 24]);
    data.extend(begin());
    let mut frame = vec![b'd'];
    frame.extend(u32::try_from(data.len() + 4)?.to_be_bytes());
    frame.extend(data);
    server.write_all(&frame).await?;
    let (sender, mut receiver) = mpsc::channel(1);
    let error = tokio::time::timeout(Duration::from_secs(1), worker.run(sender))
        .await?
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<SourceTransactionError>(),
        Some(SourceTransactionError::Deadline)
    ));
    assert!(receiver.recv().await.is_none());
    assert_eq!(worker.confirmed, 0);
    Ok(())
}
