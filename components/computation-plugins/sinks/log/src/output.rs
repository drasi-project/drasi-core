// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Emission of rendered lines to the configured output.

use crate::config::{LogLevel, LogOutput};
use anyhow::{Context, Result};
use std::{
    io::Write,
    sync::{mpsc, OnceLock},
};
use tokio::sync::oneshot;

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
    /// * `log`: one record per line at `level` with `target`, through the
    ///   `log` facade. Loaded as a library, the SDK forwards the records to
    ///   the host's logger. The facade cannot report failures, so this always
    ///   succeeds.
    /// * `stdout`/`stderr`: all lines are written with one `write_all`, each
    ///   terminated by `\n`, then flushed, on the stream's writer thread. The
    ///   input waits for that write without blocking the async runtime; a
    ///   write or flush error fails the input (transiently). Cancelling the
    ///   wait does not cancel a write already queued.
    pub async fn emit(&self, lines: &[String]) -> Result<()> {
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
            LogOutput::Stdout => StreamWriter::stdout()?
                .write(join(lines))
                .await
                .context("writing to stdout"),
            LogOutput::Stderr => StreamWriter::stderr()?
                .write(join(lines))
                .await
                .context("writing to stderr"),
        }
    }
}

fn join(lines: &[String]) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        buffer.extend_from_slice(line.as_bytes());
        buffer.push(b'\n');
    }
    buffer
}

/// Writes complete lines in one call and flushes before returning.
pub(crate) fn write_lines(writer: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}

type Request = (Vec<u8>, oneshot::Sender<std::io::Result<()>>);

/// One blocking writer thread per standard stream, shared by every log sink
/// in the process (or library). A write that blocks on a full pipe blocks
/// only this thread; waiting sinks remain cancellable.
pub(crate) struct StreamWriter(mpsc::Sender<Request>);

impl StreamWriter {
    fn stdout() -> Result<&'static Self> {
        static WRITER: OnceLock<std::io::Result<StreamWriter>> = OnceLock::new();
        Self::shared(&WRITER, "drasi-log-stdout", || {
            Box::new(std::io::stdout().lock())
        })
    }

    fn stderr() -> Result<&'static Self> {
        static WRITER: OnceLock<std::io::Result<StreamWriter>> = OnceLock::new();
        Self::shared(&WRITER, "drasi-log-stderr", || {
            Box::new(std::io::stderr().lock())
        })
    }

    fn shared(
        cell: &'static OnceLock<std::io::Result<Self>>,
        name: &str,
        stream: fn() -> Box<dyn Write>,
    ) -> Result<&'static Self> {
        cell.get_or_init(|| Self::spawn(name, stream))
            .as_ref()
            .map_err(|error| anyhow::anyhow!("cannot start the {name} writer thread: {error}"))
    }

    pub(crate) fn spawn(name: &str, stream: fn() -> Box<dyn Write>) -> std::io::Result<Self> {
        let (requests, received) = mpsc::channel::<Request>();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while let Ok((bytes, done)) = received.recv() {
                    let _ = done.send(write_lines(&mut *stream(), &bytes));
                }
            })?;
        Ok(Self(requests))
    }

    pub(crate) async fn write(&self, bytes: Vec<u8>) -> std::io::Result<()> {
        let (done, written) = oneshot::channel();
        self.0
            .send((bytes, done))
            .map_err(|_| std::io::Error::other("log writer thread has stopped"))?;
        written
            .await
            .map_err(|_| std::io::Error::other("log writer thread has stopped"))?
    }
}
