// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::super::{catalog::Column, MySqlRetention};
use super::*;
use drasi_lib::computation::v1::SourceTransactionCodec;
use std::num::NonZeroUsize;

fn format() -> FormatDescriptionEvent<'static> {
    let mut lengths = vec![0; 40];
    for (kind, length) in [
        (EventType::QUERY_EVENT, 13),
        (EventType::ROTATE_EVENT, 8),
        (EventType::TABLE_MAP_EVENT, 8),
        (EventType::WRITE_ROWS_EVENT, 10),
        (EventType::UPDATE_ROWS_EVENT, 10),
        (EventType::DELETE_ROWS_EVENT, 10),
    ] {
        lengths[kind as usize - 1] = length;
    }
    FormatDescriptionEvent::new(BinlogVersion::Version4)
        .with_event_type_header_lengths(lengths)
        .with_footer(BinlogEventFooter::new(
            BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32,
        ))
}

fn packet(kind: EventType, body: &[u8], offset: &mut u32) -> Vec<u8> {
    let size = 23 + body.len() as u32;
    *offset += size;
    let mut raw = Vec::new();
    raw.extend(1_700_000_000u32.to_le_bytes());
    raw.push(kind as u8);
    raw.extend(1u32.to_le_bytes());
    raw.extend(size.to_le_bytes());
    raw.extend(offset.to_le_bytes());
    raw.extend([0, 0]);
    raw.extend(body);
    raw.extend([0; 4]);
    let event = Event::read(&format(), raw.as_slice()).unwrap();
    let checksum = event.calc_checksum(BinlogChecksumAlg::BINLOG_CHECKSUM_ALG_CRC32);
    raw[size as usize - 4..].copy_from_slice(&checksum.to_le_bytes());
    raw
}

fn table() -> Vec<u8> {
    let mut data = vec![1, 0, 0, 0, 0, 0, 0, 0, 4];
    data.extend(b"test\0");
    data.push(5);
    data.extend(b"items\0");
    data.extend([2, 3, 15, 2, 80, 0, 0]); // two columns: INT and VARCHAR(80)
    data.extend([1, 1, 0, 2, 1, 45, 4, 9, 2]);
    data.extend(b"id");
    data.push(5);
    data.extend(b"value");
    data.extend([8, 1, 0]);
    data
}

fn frames(value: &[u8], offset: &mut u32) -> Vec<Vec<u8>> {
    let mut query = vec![0; 13];
    query[8] = 4;
    query.extend(b"test\0BEGIN");
    let mut row = vec![1, 0, 0, 0, 0, 0, 1, 0, 2, 0, 2, 3, 0];
    row.extend(1i32.to_le_bytes());
    row.push(value.len() as u8);
    row.extend(value);
    vec![
        packet(EventType::ANONYMOUS_GTID_EVENT, &[0; 25], offset),
        packet(EventType::QUERY_EVENT, &query, offset),
        packet(EventType::TABLE_MAP_EVENT, &table(), offset),
        packet(EventType::WRITE_ROWS_EVENT, &row, offset),
        packet(EventType::XID_EVENT, &1u64.to_le_bytes(), offset),
    ]
}

fn decoder(anchor: Option<Position>) -> Binlog {
    let mut config = super::super::tests::config(3306, MySqlOutput::Transactions);
    config.replay = Some(MySqlRetention::Server);
    let mut decoder = Binlog::new(Start {
        source: ComponentId::try_new("source").unwrap(),
        stream: StreamId::try_new("changes").unwrap(),
        config,
        binding: [1; 32],
        file: "mysql-bin.000001".into(),
        offset: 4,
        tables: BTreeMap::from([(
            "items".into(),
            vec![
                Column {
                    name: "id".into(),
                    sql_type: "int".into(),
                    key: true,
                    nullable: false,
                    charset: None,
                    collation: None,
                },
                Column {
                    name: "value".into(),
                    sql_type: "varchar(80)".into(),
                    key: false,
                    nullable: false,
                    charset: Some("utf8mb4".into()),
                    collation: Some(45),
                },
            ],
        )]),
        anchor,
        sequence: Arc::new(AtomicU64::new(0)),
    })
    .unwrap();
    decoder.format = format();
    decoder.described = true;
    decoder
}

#[tokio::test(flavor = "current_thread")]
async fn fast_mode_omits_transaction_assembly_and_hashing_but_bounds_decoded_batches() -> Result<()>
{
    let messages = frames(b"value", &mut 4);
    let mut fast = decoder(None);
    fast.config.output = MySqlOutput::Changes;
    fast.config.replay = None;
    fast.config.transactions.max_bytes = NonZeroUsize::new(1).unwrap();
    fast.decode(&messages[0])?;
    assert!(fast.pending.as_ref().unwrap().builder.is_none());
    assert!(fast.pending.as_ref().unwrap().digest.is_none());
    assert!(fast.deadline().is_none());
    for message in messages.iter().take(3).skip(1) {
        fast.decode(message)?;
    }
    assert_eq!(fast.decode(&messages[3])?.len(), 1);
    assert!(fast.decode(&messages[4])?.is_empty());
    let mut small = decoder(None);
    small.config.output = MySqlOutput::Changes;
    small.config.replay = None;
    small.config.max_protocol_bytes = NonZeroUsize::new(256).unwrap();
    for message in messages.iter().take(3) {
        small.decode(message)?;
    }
    assert!(small
        .decode(&messages[3])
        .unwrap_err()
        .to_string()
        .contains("decoded batch"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn replay_anchor_verifies_content_and_rejects_reused_file_positions() -> Result<()> {
    let mut first = decoder(None);
    let mut offset = 4;
    let messages = frames(b"one", &mut offset);
    let mut committed = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let output = first.decode(message)?;
        if index != 4 {
            assert!(output.is_empty());
        }
        committed.extend(output);
    }
    let transaction =
        SourceTransactionCodec::decode(&committed[0].envelope, first.config.transactions)?;
    let anchor: Position = serde_json::from_slice(transaction.position())?;
    let mut replay = decoder(Some(anchor.clone()));
    for message in &messages {
        assert!(replay.decode(message)?.is_empty());
    }
    assert!(replay.anchor.is_none());
    assert_eq!(replay.admitted, first.admitted);
    let mut changed = decoder(Some(anchor));
    for message in frames(b"two", &mut 4).iter().take(4) {
        assert!(changed.decode(message)?.is_empty());
    }
    assert!(changed
        .decode(&messages[4])
        .unwrap_err()
        .to_string()
        .contains("different contents"));
    assert!(changed.decode(&messages[0]).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_frames_metadata_and_partial_rows_fence_without_publishing() -> Result<()> {
    let messages = frames(b"valid", &mut 4);
    for end in 0..messages[0].len() {
        let mut decoder = decoder(None);
        assert!(decoder.decode(&messages[0][..end]).is_err());
        assert!(decoder.decode(&messages[0]).is_err());
    }
    for index in 0..messages[0].len() {
        let mut corrupt = messages[0].clone();
        corrupt[index] ^= 128;
        assert!(decoder(None).decode(&corrupt).is_err());
    }
    let mut metadata = table();
    metadata.extend([6, 9, 254, 255, 255, 255, 255, 255, 255, 255, 255]);
    assert!(validate_table_bytes(&metadata).is_err());
    let mut metadata = table();
    metadata.extend([4, 0]);
    assert!(validate_table_bytes(&metadata).is_err());
    let mut limited = decoder(None);
    limited.config.max_protocol_bytes = NonZeroUsize::new(23).unwrap();
    assert!(limited.decode(&messages[0]).is_err());
    let mut partial = decoder(None);
    for message in messages.iter().take(3) {
        partial.decode(message)?;
    }
    let mut offset = partial.offset;
    let body = [1, 0, 0, 0, 0, 0, 1, 0, 2, 0, 2, 1, 0, 1, 0, 0, 0];
    assert!(partial
        .decode(&packet(EventType::WRITE_ROWS_EVENT, &body, &mut offset))
        .is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_assembly_enforces_deadlines_and_transaction_payload_bounds() -> Result<()> {
    let messages = frames(b"one", &mut 4);
    let mut bytes = decoder(None);
    bytes.config.transactions.max_bytes = NonZeroUsize::new(1).unwrap();
    for message in messages.iter().take(3) {
        bytes.decode(message)?;
    }
    assert!(bytes
        .decode(&messages[3])
        .unwrap_err()
        .downcast_ref::<SourceTransactionError>()
        .is_some());
    assert!(bytes.decode(&messages[4]).is_err());
    let mut timed = decoder(None);
    timed.config.transactions.max_duration_ms = std::num::NonZeroU64::new(1).unwrap();
    timed.decode(&messages[0])?;
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    assert!(timed
        .decode(&messages[1])
        .unwrap_err()
        .downcast_ref::<SourceTransactionError>()
        .is_some());
    Ok(())
}
