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

use std::time::SystemTime;

use async_trait::async_trait;
use chrono::DateTime;
use drasi_core::{interface::{FutureElementRef, FutureQueueConsumer}, models::SourceChange};

use crate::{channels::{SourceEvent, SourceEventWrapper}, queries::PriorityQueue};

pub struct FutureConsumer {
    query_queue: PriorityQueue,
}

impl FutureConsumer {
    pub fn new(query_queue: PriorityQueue) -> Self {
        FutureConsumer {
            query_queue,
        }
    }
}

#[async_trait]
impl FutureQueueConsumer for FutureConsumer {
    async fn on_due(
        &self,
        future_ref: &FutureElementRef,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {

        log::info!(
            "Future due for {} at {}",
            future_ref.element_ref,
            future_ref.due_time
        );
        
        let evt = SourceEvent::Change(SourceChange::Future { 
            future_ref: future_ref.clone() 
        });

        let timestamp = DateTime::from_timestamp_nanos(match future_ref.due_time.try_into() {
            Ok(ts) => ts,
            Err(_) => {
                log::warn!(
                    "Due time {} for future element ref {:?} is out of range for DateTime, using current time instead",
                    future_ref.due_time,
                    future_ref
                );
                self.now() as i64
            }
        });
       
        let wrapper = SourceEventWrapper::new(future_ref.element_ref.source_id.to_string(), evt, timestamp);
        
        self.query_queue.enqueue_wait(wrapper.into()).await;
        Ok(())
    }
    async fn on_error(
        &self,
        future_ref: &FutureElementRef,
        error: Box<dyn std::error::Error + Send + Sync>,
    ) {
        log::error!(
            "Error processing {} off future queue: {:?}",
            future_ref.element_ref,
            error
        );
    }

    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            * 1000000000
    }
}