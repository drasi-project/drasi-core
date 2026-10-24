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
use std::time::Duration;
use tokio::io::{duplex, DuplexStream};

fn connection(stream: DuplexStream, limit: usize) -> ReplicationConnection<DuplexStream> {
    ReplicationConnection {
        stream,
        read_buffer: BytesMut::new(),
        write_buffer: BytesMut::new(),
        write_pending: false,
        max_message_bytes: NonZeroUsize::new(limit),
        partial_read_timeout: None,
        read_started: None,
        parameters: HashMap::new(),
        process_id: None,
        secret_key: None,
        transaction_status: TransactionStatus::Idle,
        in_copy_mode: true,
    }
}

#[tokio::test]
async fn cancelled_read_retains_partial_header_and_payload() {
    for split in 1..9 {
        let (client, mut server) = duplex(32);
        let mut connection = connection(client, 10);
        let frame = [b'd', 0, 0, 0, 9, 1, 2, 3, 4, 5];
        server.write_all(&frame[..split]).await.unwrap();
        assert!(tokio::time::timeout(
            Duration::from_millis(1),
            connection.read_replication_message()
        )
        .await
        .is_err());
        server.write_all(&frame[split..]).await.unwrap();
        let BackendMessage::CopyData(data) = connection.read_replication_message().await.unwrap()
        else {
            panic!("expected CopyData");
        };
        assert_eq!(data, [1, 2, 3, 4, 5]);
        assert!(connection.read_buffer.is_empty());
    }
}

#[tokio::test]
async fn declared_oversize_is_rejected_before_waiting_for_payload() {
    let (client, mut server) = duplex(32);
    let mut connection = connection(client, 10);
    server.write_all(&[b'd', 0, 0, 0, 10]).await.unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        connection.read_replication_message(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("10 byte limit"));
    assert_eq!(connection.read_buffer.len(), 5);
}

#[tokio::test]
async fn incomplete_frame_deadline_survives_read_cancellation_but_idle_is_allowed() {
    let (client, mut server) = duplex(32);
    let mut connection = connection(client, 10);
    connection.set_partial_read_timeout(Duration::from_millis(20));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            connection.read_replication_message(),
        )
        .await
        .is_err(),
        "idle connections are not incomplete frames"
    );
    server.write_all(&[b'd', 0, 0]).await.unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(1),
        connection.read_replication_message(),
    )
    .await
    .is_err());
    tokio::time::sleep(Duration::from_millis(25)).await;
    let error = connection.read_replication_message().await.unwrap_err();
    assert!(error
        .to_string()
        .contains("incomplete PostgreSQL wire frame timed out"));
}

#[tokio::test]
async fn cancelled_write_finishes_original_frame_before_next_frame() {
    let (client, mut server) = duplex(3);
    let mut connection = connection(client, 1024);
    let first = FrontendMessage::CopyData(vec![1, 2, 3, 4, 5]);
    let second = FrontendMessage::CopyData(vec![6, 7]);
    let mut expected = BytesMut::new();
    first.encode(&mut expected).unwrap();
    second.encode(&mut expected).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), connection.send_message(first))
            .await
            .is_err()
    );
    assert!(connection.write_pending);
    let mut actual = vec![0; expected.len()];
    let (sent, received) = tokio::join!(
        connection.send_message(second),
        server.read_exact(&mut actual)
    );
    sent.unwrap();
    received.unwrap();
    assert_eq!(actual, expected);
    assert!(!connection.write_pending);
}
