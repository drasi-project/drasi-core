// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Emission of rendered lines to the configured output.

use crate::config::{LogLevel, LogOutput};
use anyhow::{Context, Result};
use std::io::Write;

/// The settings that decide where and how lines are emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Emitter {
    pub output: LogOutput,
    pub level: LogLevel,
    pub target: String,
}

impl Emitter {
    /// Emits every line of one input.
    ///
    /// * `log`: one record per line at `level` with `target`. The `log`
    ///   facade cannot report failures, so this always succeeds.
    /// * `stdout`/`stderr`: all lines are written with one locked
    ///   `write_all`, each terminated by `\n`, then flushed. A write or flush
    ///   error fails the input.
    pub fn emit(&self, lines: &[String]) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        match self.output {
            LogOutput::Log => {
                let level = log::Level::from(self.level);
                for line in lines {
                    log::log!(target: self.target.as_str(), level, "{line}");
                }
                Ok(())
            }
            LogOutput::Stdout => {
                write_lines(&mut std::io::stdout().lock(), lines).context("writing to stdout")
            }
            LogOutput::Stderr => {
                write_lines(&mut std::io::stderr().lock(), lines).context("writing to stderr")
            }
        }
    }
}

/// Writes complete lines in one call and flushes before returning.
pub(crate) fn write_lines(writer: &mut dyn Write, lines: &[String]) -> std::io::Result<()> {
    let mut buffer = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        buffer.push_str(line);
        buffer.push('\n');
    }
    writer.write_all(buffer.as_bytes())?;
    writer.flush()
}
