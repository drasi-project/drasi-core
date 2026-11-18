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
use drasi_computation_contracts::{
    SourceProgressKey, SourceTransactionError, SourceTransactionLimits,
};
use drasi_postgres_common::{
    config::SslMode,
    connection::{parse_lsn, ReplicationConnection},
    replication::{Decoded, Position, ReplicationIo},
};
use std::{
    collections::{BTreeMap, HashMap},
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};

fn unpaused() -> PauseGate {
    PauseGate {
        requested: watch::channel(false).1,
        acknowledged: watch::channel(false).0,
    }
}
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SOURCE_ID: &str = "5c3e49cc-32f8-5b75-888d-30b75b572221";

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

fn transactions() -> Transactions {
    Transactions::new(
        strict(),
        BTreeMap::from([(("public".into(), "person".into()), vec!["id".into()])]),
        ComponentId::try_new(SOURCE_ID).unwrap(),
        [1; 32],
        None,
        0,
        false,
    )
}

fn stream() -> StreamId {
    StreamId::try_new("stream").unwrap()
}

fn limits(changes: usize) -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: NonZeroUsize::new(changes).unwrap(),
        max_bytes: NonZeroUsize::new(65536).unwrap(),
        max_duration_ms: NonZeroU64::new(1000).unwrap(),
    }
}

fn whole(limits: SourceTransactionLimits, cursors: Cursors) -> WholeEmission {
    WholeEmission::new(
        ComponentId::try_new(SOURCE_ID).unwrap(),
        stream(),
        limits,
        Arc::new(AtomicU64::new(0)),
        cursors,
    )
}

/// Feeds pgoutput frames to row emission, as the worker does.
fn feed_rows(
    transactions: &mut Transactions,
    rows: &mut RowEmission,
    frames: &[Vec<u8>],
    idle: bool,
) -> Result<VecDeque<OutputEnvelope>> {
    let mut output = VecDeque::new();
    for frame in frames {
        match transactions.decode(frame)? {
            Some(Decoded::Begin(info)) => rows.begin(&info)?,
            Some(Decoded::Changes(changes)) => rows.push(changes, transactions, &mut output)?,
            Some(Decoded::Commit(info)) => rows.commit(&info, idle, transactions, &mut output)?,
            None => {}
        }
    }
    Ok(output)
}

/// Feeds pgoutput frames to whole-transaction emission, as the worker does.
fn feed_whole(
    transactions: &mut Transactions,
    whole: &mut WholeEmission,
    frames: &[Vec<u8>],
    idle: bool,
) -> Result<Option<OutputEnvelope>> {
    let mut output = None;
    for frame in frames {
        match transactions.decode(frame)? {
            Some(Decoded::Begin(_)) => whole.begin()?,
            Some(Decoded::Changes(changes)) => whole.push(changes)?,
            Some(Decoded::Commit(info)) => {
                output = whole.commit(
                    info.commit_lsn,
                    info.commit_timestamp,
                    idle,
                    transactions.binding,
                    None,
                )?;
            }
            None => {}
        }
    }
    Ok(output)
}

fn transaction() -> Vec<Vec<u8>> {
    vec![
        begin(),
        relation(),
        insert(b"1", b"one"),
        insert(b"2", b"two"),
        commit(),
    ]
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
async fn whole_transaction_mapping_is_stable_and_never_publishes_a_prefix() -> Result<()> {
    let limits = limits(1);
    let mut payloads = Vec::new();
    for _ in 0..2 {
        let mut transactions = transactions();
        let mut emission = whole(limits, Cursors::Single);
        let output = feed_whole(
            &mut transactions,
            &mut emission,
            &[begin(), relation(), insert(b"1", b"final"), commit()],
            false,
        )?
        .context("committed transaction")?;
        let transaction = SourceTransactionCodec::decode(&output.envelope, limits)?;
        let changes = transaction.into_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].get_transaction_time(), 946_684_801_000);
        payloads.push(serde_json::to_vec(&changes)?);
    }
    assert_eq!(payloads[0], payloads[1]);
    let mut transactions = transactions();
    let mut emission = whole(limits, Cursors::Single);
    feed_whole(
        &mut transactions,
        &mut emission,
        &[begin(), relation(), insert(b"1", b"first")],
        false,
    )?;
    let error = feed_whole(
        &mut transactions,
        &mut emission,
        &[insert(b"2", b"second")],
        false,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<SourceTransactionError>().is_some());
    assert!(
        feed_whole(&mut transactions, &mut emission, &[commit()], false).is_err(),
        "rejected prefix must never publish"
    );
    Ok(())
}

#[tokio::test]
async fn empty_commits_are_progress_records_only_when_idle() -> Result<()> {
    let limits = limits(16);
    let empty = [begin(), relation(), commit()];
    let mut transactions = transactions();
    let mut emission = whole(limits, Cursors::Single);
    assert!(feed_whole(&mut transactions, &mut emission, &empty, false)?.is_none());
    assert_eq!(
        transactions.resume, 100,
        "a dropped empty commit is still handled"
    );
    let mut transactions = self::transactions();
    let output = feed_whole(&mut transactions, &mut emission, &empty, true)?
        .context("an idle empty commit is a progress record")?;
    let transaction = SourceTransactionCodec::decode(&output.envelope, limits)?;
    let position: Position = serde_json::from_slice(transaction.position())?;
    assert_eq!(position.commit_lsn, 100);
    assert!(transaction.into_changes().is_empty());

    let mut transactions = self::transactions();
    let mut rows = RowEmission::new(stream(), None, 0, Arc::new(AtomicU64::new(0)), limits);
    assert!(feed_rows(&mut transactions, &mut rows, &empty, false)?.is_empty());
    let mut transactions = self::transactions();
    let output = feed_rows(&mut transactions, &mut rows, &empty, true)?;
    assert_eq!(output.len(), 1);
    assert!(output[0].envelope.changes().operations().is_empty());
    let position = row_cursor(&output[0])?;
    assert!(position.complete() && position.offset.is_none());
    assert_eq!((position.commit_lsn, position.sequence), (100, 1));
    Ok(())
}

/// The row cursor an output carries.
fn row_cursor(
    output: &OutputEnvelope,
) -> Result<drasi_postgres_common::replication::rows::RowPosition> {
    let metadata = GraphChangeCodec::source_metadata(&output.envelope)?.context("row metadata")?;
    let position: drasi_postgres_common::replication::rows::RowPosition =
        serde_json::from_slice(&metadata.source_position.context("row position")?)?;
    assert_eq!(metadata.sequence, Some(position.sequence));
    Ok(position)
}

#[tokio::test]
async fn an_oversized_row_transaction_streams_and_its_commit_completes_it() -> Result<()> {
    use drasi_core_types::interface::SourceCheckpoint;
    let limits = limits(2);
    let frames = [
        begin(),
        relation(),
        insert(b"1", b"one"),
        insert(b"2", b"two"),
        insert(b"3", b"three"),
        commit(),
    ];
    let mut transactions = transactions();
    let mut rows = RowEmission::new(stream(), None, 0, Arc::new(AtomicU64::new(0)), limits);
    // The first two rows buffer; the third exceeds the limit and streams all.
    let streamed = feed_rows(&mut transactions, &mut rows, &frames[..5], false)?;
    assert_eq!(streamed.len(), 3, "rows stream before the commit");
    assert!(rows.at_boundary() && rows.deadline().is_none());
    let cursors = streamed
        .iter()
        .map(row_cursor)
        .collect::<Result<Vec<_>>>()?;
    assert!(cursors
        .iter()
        .all(|cursor| cursor.is_open() && cursor.feedback_lsn() == 0));
    // A consumer may commit a streamed row before its commit arrives.
    let mut streamed = SourceProgressSnapshot::default();
    streamed.checkpoints.insert(
        SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID)?),
        SourceCheckpoint::new(
            cursors[2].sequence,
            Some(serde_json::to_vec(&cursors[2])?.into()),
        ),
    );
    assert_eq!(rows.confirmed(&streamed, &transactions)?, Some(0));
    let completion = feed_rows(&mut transactions, &mut rows, &frames[5..], false)?;
    assert_eq!(completion.len(), 1);
    assert!(completion[0].envelope.changes().operations().is_empty());
    let complete = row_cursor(&completion[0])?;
    assert!(complete.complete());
    assert_eq!((complete.commit_lsn, complete.sequence), (100, 4));

    // A restart after the second streamed row replays the third, then the
    // completion record, even with a raised limit that now buffers it.
    let mut progress = SourceProgressSnapshot::default();
    progress.checkpoints.insert(
        SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID)?),
        SourceCheckpoint::new(
            cursors[1].sequence,
            Some(serde_json::to_vec(&cursors[1])?.into()),
        ),
    );
    assert_eq!(rows.confirmed(&progress, &transactions)?, Some(0));
    let mut replay = RowEmission::new(
        stream(),
        Some(cursors[1].clone()),
        0,
        Arc::new(AtomicU64::new(10)),
        self::limits(16),
    );
    let mut transactions = self::transactions();
    transactions.resume = 99;
    let output = feed_rows(&mut transactions, &mut replay, &frames, false)?;
    assert_eq!(output.len(), 2);
    let third = row_cursor(&output[0])?;
    assert_eq!((third.offset, third.count, third.sequence), (Some(2), 3, 3));
    assert!(row_cursor(&output[1])?.complete());
    Ok(())
}

#[tokio::test]
async fn runtime_truncate_of_a_selected_table_is_a_typed_unsupported_failure() -> Result<()> {
    use drasi_computation_contracts::{source_failure, SourceFailure};
    let truncate = |relation: u32| {
        let mut bytes = vec![b'T'];
        bytes.extend(1u32.to_be_bytes());
        bytes.push(0);
        bytes.extend(relation.to_be_bytes());
        bytes
    };
    let mut transactions = transactions();
    let mut emission = whole(limits(16), Cursors::Single);
    let error = feed_whole(
        &mut transactions,
        &mut emission,
        &[begin(), relation(), insert(b"1", b"one"), truncate(1)],
        false,
    )
    .unwrap_err();
    assert!(
        matches!(source_failure(&error), Some(SourceFailure::Unsupported(_))),
        "{error:#}"
    );
    // A TRUNCATE of a published but unselected table changes nothing here.
    let mut other = relation();
    other[1..5].copy_from_slice(&2u32.to_be_bytes());
    let name = other
        .windows(7)
        .position(|window| window == b"person\0")
        .unwrap();
    other.splice(name..name + 6, *b"others");
    let mut transactions = self::transactions();
    transactions.filtered = true;
    let mut emission = whole(limits(16), Cursors::Single);
    let output = feed_whole(
        &mut transactions,
        &mut emission,
        &[
            begin(),
            other,
            relation(),
            insert(b"1", b"one"),
            truncate(2),
            commit(),
        ],
        false,
    )?
    .context("the transaction still commits")?;
    assert_eq!(
        SourceTransactionCodec::decode(&output.envelope, limits(16))?
            .into_changes()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn shared_transaction_positions_are_allocated_only_for_complete_commits() -> Result<()> {
    let limits = limits(2);
    let mut decoder = transactions();
    let transport = Arc::new(AtomicU64::new(0));
    let cursors = TransactionEmission::new(Arc::default(), transport.clone(), 20_000, 0);
    let mut emission = WholeEmission::new(
        ComponentId::try_new(SOURCE_ID)?,
        stream(),
        limits,
        transport,
        Cursors::Shared(cursors),
    );
    assert!(feed_whole(
        &mut decoder,
        &mut emission,
        &[
            begin(),
            relation(),
            insert(b"1", b"first"),
            insert(b"2", b"second"),
        ],
        false,
    )?
    .is_none());
    let output = feed_whole(&mut decoder, &mut emission, &[commit()], false)?
        .context("complete transaction")?;
    let transaction = SourceTransactionCodec::decode(&output.envelope, limits)?;
    let position: drasi_postgres_common::replication::shared_transactions::SharedTransactionPosition =
        serde_json::from_slice(transaction.position())?;
    assert_eq!(position.sequence, 20_001);
    assert_eq!(output.envelope.system().sequence(), 20_001);
    assert_eq!(position.commit_lsn, 100);
    assert_eq!(transaction.transaction_id(), 100u64.to_be_bytes());
    assert_eq!(transaction.into_changes().len(), 2);
    Ok(())
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

    let source = ComponentId::try_new(SOURCE_ID).unwrap();
    let key = SourceProgressKey::Source(
        drasi_core_types::ComponentId::try_new("5c3e49cc-32f8-5b75-888d-30b75b572221")
            .expect("fixture UUID"),
    );
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
        "database": "database",
        "user": "user",
        "publication": "publication",
        "slotOwnership": "external",
        "slotName": "slot",
        "startLsn": "0/10",
        "maxTransactionChanges": 16,
        "maxTransactionBytes": 4096,
        "maxTransactionDurationMs": 1000,
        "maxProtocolBytes": 4096,
        "ioTimeoutMs": 1000,
        "feedbackIntervalMs": 100,
    });
    let config: PostgresTransactionConfig = serde_json::from_value(valid.clone()).unwrap();
    config.validate().unwrap();
    let valid = serde_json::to_value(&config).unwrap();
    for (path, value) in [
        ("/host", serde_json::json!("host\0suffix")),
        ("/database", serde_json::json!("database\0suffix")),
        ("/user", serde_json::json!("user\0suffix")),
        ("/password", serde_json::json!("password\0suffix")),
        ("/database", serde_json::json!("")),
        ("/maxProtocolBytes", serde_json::json!(63)),
        ("/startLsn", serde_json::json!("100000000/0")),
        ("/reconnectBudgetMs", serde_json::json!(86_400_001u64)),
    ] {
        let mut invalid = valid.clone();
        invalid[&path[1..]] = value;
        let config: PostgresTransactionConfig = serde_json::from_value(invalid).unwrap();
        assert!(config.validate().is_err(), "{path}");
    }
    for field in [
        "maxTransactionChanges",
        "maxTransactionBytes",
        "maxTransactionDurationMs",
        "maxProtocolBytes",
        "ioTimeoutMs",
        "feedbackIntervalMs",
    ] {
        let mut invalid = valid.clone();
        invalid[field] = serde_json::json!(0);
        assert!(serde_json::from_value::<PostgresTransactionConfig>(invalid).is_err());
    }
    for slot in ["UPPERCASE", "unsafe';", &"a".repeat(64)] {
        let config = config.clone().with_external_slot(slot);
        assert!(config.validate().is_err());
    }
    for field in ["hostname", "ssl_mode", "password_typo", "connection"] {
        let mut invalid = valid.clone();
        invalid[field] = serde_json::json!("must not silently use a default");
        assert!(
            serde_json::from_value::<PostgresTransactionConfig>(invalid).is_err(),
            "{field}"
        );
    }
}

#[test]
fn managed_handover_requires_a_completed_owner_cursor_and_exact_slot_prefix() -> Result<()> {
    use drasi_core_types::interface::SourceCheckpoint;
    let source = ComponentId::try_new(SOURCE_ID)?;
    let mut config = PostgresTransactionConfig::new("host", 5432, "database", "user", "pub");
    config.resolve("graph", &source)?;
    let owned = config.managed_prefix()?.to_owned();
    let mut progress = SourceProgressSnapshot::default();
    assert!(live_config(&config, &progress, &source).is_err());
    progress.bootstrap_complete = true;
    assert!(live_config(&config, &progress, &source).is_err());
    for slot in [
        None,
        Some(owned.clone()),
        Some(format!("someone_else_{}", "a".repeat(32))),
        Some(format!("{owned}_{}", "A".repeat(32))),
        Some(format!("{owned}_{}';", "a".repeat(32))),
    ] {
        progress.checkpoints.insert(
            SourceProgressKey::Source(
                drasi_core_types::ComponentId::try_new("5c3e49cc-32f8-5b75-888d-30b75b572221")
                    .expect("fixture UUID"),
            ),
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

    let slot = format!("{owned}_{}", "a".repeat(32));
    progress.checkpoints.insert(
        SourceProgressKey::Source(
            drasi_core_types::ComponentId::try_new("5c3e49cc-32f8-5b75-888d-30b75b572221")
                .expect("fixture UUID"),
        ),
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
    assert_eq!(recovered.slot_name(), slot);
    assert_eq!(recovered.start_lsn()?, Some(100));
    assert!(
        !recovered.creates_slot(),
        "a handed-over slot is never created"
    );
    progress.bootstrap_complete = false;
    assert!(live_config(&config, &progress, &source).is_err());
    Ok(())
}

#[tokio::test]
async fn ordinary_rows_preserve_partial_replay_and_confirm_only_complete_transactions() -> Result<()>
{
    use drasi_core_types::interface::SourceCheckpoint;
    use drasi_postgres_common::replication::rows::RowPosition;
    let limits = limits(16);
    let mut transaction = transactions();
    let mut rows = RowEmission::new(stream(), None, 0, Arc::new(AtomicU64::new(0)), limits);
    let output = feed_rows(&mut transaction, &mut rows, &self::transaction(), false)?;
    assert_eq!(output.len(), 2);
    let mut progress = SourceProgressSnapshot::default();
    let first = GraphChangeCodec::source_metadata(&output[0].envelope)?.context("row metadata")?;
    let first_position = first.source_position.context("row position")?;
    progress.checkpoints.insert(
        SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID)?),
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
    let mut replacement = RowEmission::new(
        stream(),
        Some(resume),
        0,
        Arc::new(AtomicU64::new(10)),
        limits,
    );
    let mut transaction = transactions();
    let replay = feed_rows(
        &mut transaction,
        &mut replacement,
        &self::transaction(),
        false,
    )?;
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].envelope.system().sequence(), 11);
    let second =
        GraphChangeCodec::source_metadata(&replay[0].envelope)?.context("replayed row metadata")?;
    assert_eq!(second.sequence, Some(2));
    progress.checkpoints.insert(
        SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID)?),
        SourceCheckpoint::new(
            2,
            Some(second.source_position.context("row position")?.into()),
        ),
    );
    assert_eq!(replacement.confirmed(&progress, &transaction)?, Some(100));
    Ok(())
}

#[tokio::test]
async fn shared_rows_keep_all_counter_floors_across_rewind_and_worker_replacement() -> Result<()> {
    use crate::shared::SharedCounters;
    use drasi_postgres_common::replication::rows::RowPosition;
    let limits = limits(16);
    let partial = RowPosition {
        version: 1,
        binding: [1; 32],
        commit_lsn: 100,
        previous_lsn: 0,
        offset: Some(0),
        count: 2,
        sequence: 2,
        managed_slot: None,
    };
    let emit = |rows: &mut RowEmission| -> Result<_> {
        let mut transaction = transactions();
        transaction.resume = 99;
        feed_rows(&mut transaction, rows, &self::transaction(), false)
    };
    let counters = Arc::new(SharedCounters::default());
    let transport = Arc::new(AtomicU64::new(10_000));
    let mut rows = RowEmission::shared(
        stream(),
        Some(partial.clone()),
        0,
        transport.clone(),
        limits,
        counters.clone(),
        8_000,
        200,
        Default::default(),
    );
    for expected in [10_001, 10_002] {
        let output = emit(&mut rows)?;
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.system().sequence(), expected);
        let metadata =
            GraphChangeCodec::source_metadata(&output[0].envelope)?.context("metadata")?;
        assert_eq!(metadata.sequence, Some(expected));
        let position: RowPosition =
            serde_json::from_slice(&metadata.source_position.context("position")?)?;
        assert_eq!(position.offset, Some(1));
        assert_eq!(position.count, 2);
        assert_eq!(
            rows.admitted_lsn(),
            200,
            "rewind must retain ahead-member/queued admission bounds"
        );
        rows.validate_shared_observation(expected, 200)?;
        assert!(rows.validate_shared_observation(expected + 1, 200).is_err());
        assert!(rows.validate_shared_observation(expected, 201).is_err());
        rows.rewind_shared(Some(partial.clone()), 0, 2, 100, Default::default())?;
    }
    let mut replacement = RowEmission::shared(
        stream(),
        Some(partial.clone()),
        0,
        transport.clone(),
        limits,
        counters,
        2,
        100,
        Default::default(),
    );
    assert_eq!(
        emit(&mut replacement)?[0].envelope.system().sequence(),
        10_003
    );
    replacement.rewind_shared(Some(partial), 0, u64::MAX, 200, Default::default())?;
    assert!(emit(&mut replacement)
        .unwrap_err()
        .to_string()
        .contains("sequence exhausted"));
    Ok(())
}

#[tokio::test]
async fn partial_rows_preserve_a_later_subscribers_real_snapshot_boundary() -> Result<()> {
    use crate::shared::SharedCounters;
    let mut transaction = transactions();
    let mut rows = RowEmission::shared(
        stream(),
        None,
        50,
        Arc::new(AtomicU64::new(0)),
        limits(16),
        Arc::new(SharedCounters::default()),
        0,
        110,
        [95, 100, 110].into_iter().collect(),
    );
    let output = feed_rows(&mut transaction, &mut rows, &self::transaction(), false)?;
    let partial = row_cursor(&output[0])?;
    assert!(!partial.complete());
    assert_eq!(
        partial.feedback_lsn(),
        95,
        "moving beyond a snapshot must not regress its complete-coverage floor"
    );
    assert_eq!(partial.commit_lsn, 100);
    assert!(
        partial.feedback_lsn() < partial.commit_lsn,
        "an equal/future snapshot cannot acknowledge an unhandled suffix"
    );
    Ok(())
}

#[tokio::test]
async fn a_group_join_record_commits_the_slowest_cursor_with_a_fresh_sequence() -> Result<()> {
    use crate::shared::SharedCounters;
    use drasi_postgres_common::replication::rows::RowPosition;
    let partial = RowPosition {
        version: 1,
        binding: [1; 32],
        commit_lsn: 100,
        previous_lsn: 0,
        offset: Some(0),
        count: 2,
        sequence: 7,
        managed_slot: None,
    };
    let transaction = transactions();
    let mut rows = RowEmission::shared(
        stream(),
        Some(partial.clone()),
        0,
        Arc::new(AtomicU64::new(0)),
        limits(16),
        Arc::new(SharedCounters::default()),
        7,
        100,
        Default::default(),
    );
    // A fresh member joins at the slowest member's partial cursor: the rest
    // of that transaction follows for both.
    let join = row_cursor(&rows.join(Some(&partial), 100, &transaction)?)?;
    assert_eq!(
        (join.commit_lsn, join.offset, join.count),
        (100, Some(0), 2)
    );
    assert!(join.sequence > partial.sequence);
    // A complete minimum, or a fresh origin, is joined by a progress record.
    let origin = row_cursor(&rows.join(None, 80, &transaction)?)?;
    assert!(origin.complete() && origin.offset.is_none());
    assert_eq!(origin.commit_lsn, 80);
    assert!(origin.sequence > join.sequence);
    Ok(())
}

/// A worker connected to a scripted replication server.
async fn scripted_worker(
    limits: SourceTransactionLimits,
    emission: Emission,
    progress: watch::Receiver<Arc<SourceProgressSnapshot>>,
) -> Result<(Worker, tokio::io::DuplexStream)> {
    let (client, mut server) = tokio::io::duplex(1 << 16);
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
    let mut config =
        PostgresTransactionConfig::new("unused", 5432, "database", "user", "publication")
            .with_external_slot("slot");
    config.ssl_mode = SslMode::Disable;
    config.start_lsn = Some("0/0".into());
    config.max_transaction_changes = limits.max_changes;
    config.max_transaction_bytes = limits.max_bytes;
    config.max_transaction_duration_ms = limits.max_duration_ms;
    config.max_protocol_bytes = NonZeroUsize::new(4096).unwrap();
    config.io_timeout_ms = NonZeroU64::new(1000).unwrap();
    config.feedback_interval_ms = NonZeroU64::new(10).unwrap();
    config.reconnect_budget_ms = 0;
    let worker = Worker {
        connection: Some(connection?),
        stream: stream(),
        transactions: transactions(),
        progress: WorkerProgress::Single {
            updates: Box::new(progress),
            generation: 0,
        },
        confirmed: 0,
        next_feedback: Instant::now(),
        config,
        emission,
        queue: VecDeque::new(),
        holding: false,
        origin: None,
        last_emission: Instant::now(),
        idle_progress: Duration::ZERO,
        pause: unpaused(),
        reconnect_budget_ms: Arc::new(AtomicU64::new(0)),
    };
    Ok((worker, server?))
}

/// One CopyData frame carrying `data`.
fn copy_data(data: &[u8]) -> Vec<u8> {
    let mut frame = vec![b'd'];
    frame.extend(u32::try_from(data.len() + 4).unwrap().to_be_bytes());
    frame.extend(data);
    frame
}

fn xlog(payload: &[u8]) -> Vec<u8> {
    let mut data = vec![b'w'];
    data.extend([0; 24]);
    data.extend(payload);
    copy_data(&data)
}

fn keepalive(wal_end: u64) -> Vec<u8> {
    let mut data = vec![b'k'];
    data.extend(wal_end.to_be_bytes());
    data.extend(0i64.to_be_bytes());
    data.push(0);
    copy_data(&data)
}

fn ready_progress() -> (
    watch::Sender<Arc<SourceProgressSnapshot>>,
    watch::Receiver<Arc<SourceProgressSnapshot>>,
) {
    watch::channel(Arc::new(SourceProgressSnapshot {
        ready: true,
        persistent: true,
        ..Default::default()
    }))
}

#[tokio::test]
async fn native_worker_enforces_idle_transaction_deadline_without_publishing() -> Result<()> {
    let limits = SourceTransactionLimits {
        max_duration_ms: NonZeroU64::new(25).unwrap(),
        ..limits(16)
    };
    let (_progress, watch) = ready_progress();
    let (mut worker, mut server) = scripted_worker(
        limits,
        Emission::Whole(whole(limits, Cursors::Single)),
        watch,
    )
    .await?;
    server.write_all(&xlog(&begin())).await?;
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

/// While the selected tables are quiet, a keepalive's WAL end minus one is
/// committed as a progress record in every output, and feedback follows the
/// consumer's commit of it. A keepalive inside the quiet interval, or at a
/// position already handled, emits nothing.
#[tokio::test]
async fn quiet_keepalives_emit_progress_records_that_advance_feedback() -> Result<()> {
    use drasi_core_types::interface::SourceCheckpoint;
    let limits = limits(16);
    for rows in [false, true] {
        let emission = if rows {
            Emission::Rows(RowEmission::new(
                stream(),
                None,
                0,
                Arc::new(AtomicU64::new(0)),
                limits,
            ))
        } else {
            Emission::Whole(whole(limits, Cursors::Single))
        };
        let (progress, watch) = ready_progress();
        let (mut worker, mut server) = scripted_worker(limits, emission, watch).await?;
        server.write_all(&keepalive(0x500)).await?;
        let (sender, mut receiver) = mpsc::channel(4);
        let run = tokio::spawn(async move {
            let result = worker.run(sender).await;
            (worker, result)
        });
        let record = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await?
            .context("a progress record")?;
        assert!(record.envelope.changes().operations().is_empty());
        let (sequence, position) = if rows {
            let cursor = row_cursor(&record)?;
            assert_eq!(cursor.commit_lsn, 0x4FF);
            (cursor.sequence, serde_json::to_vec(&cursor)?)
        } else {
            let transaction = SourceTransactionCodec::decode(&record.envelope, limits)?;
            let position: Position = serde_json::from_slice(transaction.position())?;
            assert_eq!(position.commit_lsn, 0x4FF);
            (0x4FF, transaction.position().to_vec())
        };
        // The same WAL end again is already handled.
        server.write_all(&keepalive(0x500)).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), receiver.recv())
                .await
                .is_err(),
            "nothing new to commit"
        );
        // The consumer commits the record; standby feedback reports it.
        let mut committed = SourceProgressSnapshot {
            ready: true,
            persistent: true,
            ..Default::default()
        };
        committed.checkpoints.insert(
            SourceProgressKey::Source(ComponentId::try_new(SOURCE_ID)?),
            SourceCheckpoint::new(sequence, Some(position.into())),
        );
        progress.send_replace(Arc::new(committed));
        let flushed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                assert_eq!(server.read_u8().await?, b'd');
                let length = server.read_u32().await? as usize;
                let mut data = vec![0; length - 4];
                server.read_exact(&mut data).await?;
                if data[0] == b'r' {
                    let flush = u64::from_be_bytes(data[9..17].try_into()?);
                    if flush == 0x4FF {
                        return Result::<u64>::Ok(flush);
                    }
                }
            }
        })
        .await??;
        assert_eq!(flushed, 0x4FF);
        drop(server);
        let (_worker, result) = run.await?;
        assert!(result.is_err(), "the scripted server closed");
    }
    Ok(())
}
