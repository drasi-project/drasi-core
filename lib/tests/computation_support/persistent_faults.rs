// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{IndexError, IndexSet, SessionControl},
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
};

pub fn provider(path: &Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("RocksDB budget"),
        ),
    ))
}

#[derive(Default)]
pub struct Fault {
    // 1: fail before commit; 2: lose reply; 3/4: hold after/before the actual commit.
    pub mode: AtomicU8,
    pub skip: AtomicUsize,
    pub committed: tokio::sync::Notify,
}

struct FaultSession {
    inner: Arc<dyn SessionControl>,
    fault: Arc<Fault>,
}

#[async_trait]
impl SessionControl for FaultSession {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let mode = if self
            .fault
            .skip
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |skip| {
                skip.checked_sub(1)
            })
            .is_ok()
        {
            0
        } else {
            self.fault.mode.swap(0, Ordering::SeqCst)
        };
        if mode == 1 {
            return Err(IndexError::IOError);
        }
        if mode == 4 {
            self.fault.committed.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.commit().await?;
        if mode == 2 {
            return Err(IndexError::IOError);
        }
        if mode == 3 {
            self.fault.committed.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

pub struct FaultProvider {
    pub inner: Arc<dyn ComputationIndexProvider>,
    pub fault: Arc<Fault>,
}

#[async_trait]
impl ComputationIndexProvider for FaultProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, id).await?;
        let set = original.indexes();
        let session: Arc<dyn SessionControl> = Arc::new(FaultSession {
            inner: set.session_control.clone(),
            fault: self.fault.clone(),
        });
        let domain = TransactionDomain::new(session.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: session,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().expect("checkpoint").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().expect("projection").clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(indexes
            .with_cleanup(original.cleanup().expect("cleanup owner").clone())
            .with_durability(original.durability()))
    }

    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

#[cfg(unix)]
pub mod process {
    use serde_json::{json, Value};
    use std::{
        fs::File,
        os::unix::process::ExitStatusExt,
        path::Path,
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    pub const WATCHDOG: Duration = Duration::from_secs(20);

    pub fn write_boundary(path: &Path, phase: &str, graph: &str) {
        let pending = path.join("boundary.pending.json");
        std::fs::write(
            &pending,
            serde_json::to_vec(&json!({
                "pid": std::process::id(),
                "phase": phase,
                "graph": graph,
            }))
            .expect("serialize child boundary identity"),
        )
        .expect("write pending child boundary");
        std::fs::rename(pending, path.join("boundary.json")).expect("publish child boundary");
    }

    pub struct OwnedChild {
        child: Child,
        graph: String,
    }

    impl OwnedChild {
        pub fn start(path: &Path, worker: &str, phase: &str, graph: &str) -> Self {
            Self {
                child: Command::new(std::env::current_exe().expect("current test executable"))
                    .args(["--ignored", "--exact", worker, "--nocapture"])
                    .env("DRASI_CHILD_STATE", path)
                    .env("DRASI_CHILD_PHASE", phase)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(
                        File::create(path.join(format!("{phase}.log")))
                            .expect("create child diagnostic log"),
                    )
                    .spawn()
                    .expect("spawn actual DrasiLib child"),
                graph: graph.into(),
            }
        }

        pub fn id(&self) -> u32 {
            self.child.id()
        }

        pub fn wait(&mut self, path: &Path, phase: &str, boundary: bool) {
            let deadline = Instant::now() + WATCHDOG;
            loop {
                if let Some(status) = self.child.try_wait().expect("inspect owned child exit") {
                    assert!(
                        !boundary && status.success(),
                        "{phase} exited {status}: {}",
                        std::fs::read_to_string(path.join(format!("{phase}.log")))
                            .expect("read child exit diagnostics")
                    );
                    break;
                }
                if boundary && path.join("boundary.json").exists() {
                    let value: Value = serde_json::from_slice(
                        &std::fs::read(path.join("boundary.json")).expect("read child boundary"),
                    )
                    .expect("decode child boundary identity");
                    assert_eq!(value["pid"], self.id());
                    assert_eq!(value["phase"], phase);
                    assert_eq!(value["graph"], self.graph);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{phase} external watchdog expired: {}",
                    std::fs::read_to_string(path.join(format!("{phase}.log")))
                        .expect("read child watchdog diagnostics")
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        pub fn kill(&mut self) {
            self.child.kill().expect("SIGKILL the owned child");
            assert_eq!(
                self.child.wait().expect("reap the killed child").signal(),
                Some(9)
            );
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}
