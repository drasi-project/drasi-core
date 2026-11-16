// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Typed admission outcomes of the native SQLite worker.

use std::fmt;

/// A write was refused because the source is paused. Nothing was executed;
/// the caller may retry after the source resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlitePaused;

impl fmt::Display for SqlitePaused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native SQLite source is paused; writes are refused until it resumes")
    }
}

impl std::error::Error for SqlitePaused {}

/// A queued command was refused because the source is stopping. Nothing was
/// executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteStopping;

impl fmt::Display for SqliteStopping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("native SQLite source is stopping; the command was not executed")
    }
}

impl std::error::Error for SqliteStopping {}

/// Whether `error` reports that the source refused work because it is paused.
pub fn is_paused(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<SqlitePaused>().is_some())
}

/// Whether `error` reports that the source refused work because it is stopping.
pub fn is_stopping(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<SqliteStopping>().is_some())
}
