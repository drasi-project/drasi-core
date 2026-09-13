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

use super::*;
use drasi_lib::channels::QueryResult;
use std::time::Duration;

struct ReportLogger {
    first_report: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

impl log::Log for ReportLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        let message = record.args().to_string();
        if message.starts_with("[profiler-report-fairness] Source") {
            if let Some(tx) = self.first_report.lock().unwrap().take() {
                tx.send(message).unwrap();
            }
        }
    }

    fn flush(&self) {}
}

#[tokio::test]
async fn a_due_report_is_not_starved_by_queued_results() {
    static LOGGER: ReportLogger = ReportLogger {
        first_report: std::sync::Mutex::new(None),
    };
    let (report_tx, report_rx) = tokio::sync::oneshot::channel();
    *LOGGER.first_report.lock().unwrap() = Some(report_tx);
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    let reaction = ProfilerReaction::new(
        "profiler-report-fairness",
        vec!["q1".to_string()],
        ProfilerReactionConfig {
            report_interval_secs: 1,
            ..Default::default()
        },
    );
    reaction.start().await.unwrap();
    let held_stats = reaction.stats.write().await;
    let mut result = QueryResult::new(
        "q1".to_string(),
        1,
        chrono::Utc::now(),
        vec![],
        Default::default(),
    );
    result.profiling = Some(ProfilingMetadata::default());
    reaction.enqueue_query_result(result.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while reaction.base.priority_queue.metrics().await.total_dequeued == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for sequence in 2..=65 {
        result.sequence = sequence;
        reaction.enqueue_query_result(result.clone()).await.unwrap();
    }
    // The consumer is gated on its first sample. Let its real timer become due
    // with a full backlog, then verify the report runs before another dequeue.
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(held_stats);
    let report = tokio::time::timeout(Duration::from_secs(30), report_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while reaction.stats.read().await.count < 65 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), reaction.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(
        report.ends_with("(n=1)"),
        "report was delayed by backlog: {report}"
    );
}

#[tokio::test]
async fn stop_and_restart_reopens_the_result_queue() {
    let reaction = ProfilerReaction::new(
        "profiler-restart",
        vec!["q1".to_string()],
        ProfilerReactionConfig::default(),
    );
    for sequence in [100, 1] {
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(QueryResult::new(
                "q1".to_string(),
                sequence,
                chrono::Utc::now(),
                vec![],
                Default::default(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !reaction.base.priority_queue.is_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), reaction.stop())
            .await
            .expect("profiler must use graceful shutdown, not the two-second abort timeout")
            .unwrap();
        assert!(reaction.base.processing_task.read().await.is_none());
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }
}
