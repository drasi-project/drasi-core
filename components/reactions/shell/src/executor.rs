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

#![cfg(target_os = "linux")]

use drasi_lib::channels::{PriorityQueue, QueryResult, ResultDiff};
use drasi_lib::reactions::common::TemplateRouting;
use tokio::process::Child;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::config::{ShellCommand, ShellExtension, ShellReactionConfig};
use crate::state::{
    self, CommandExecutionContext, RecentInvocation, RecentInvocations, ShellReactionMetrics,
    ShellReactionProcessState, ShellReactionState,
};
use chrono::{DateTime, Utc};
use drasi_lib::reactions::common::OperationType;
use handlebars::Handlebars;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::process;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::oneshot::Receiver;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use std::process::Stdio;

use log::{debug, error, info, warn};

#[derive(Debug)]
pub struct ShellExecutor {
    reaction_id: String,
    pub state: Arc<ShellReactionState>,
    pub config: Arc<ShellReactionConfig>,
}

impl ShellExecutor {
    pub fn new(reaction_id: String, config: ShellReactionConfig) -> Self {
        Self {
            reaction_id: reaction_id.clone(),
            state: Arc::new(ShellReactionState {
                recent_invocations: Mutex::new(RecentInvocations::new(
                    config.max_recent_invocations,
                )),
                metrics: ShellReactionMetrics::new(),
                process_state: ShellReactionProcessState::new(),
                reaction_id: reaction_id.clone(),
            }),
            config: Arc::new(config),
        }
    }

    pub async fn start_processing_loop(
        &self,
        priority_queue: PriorityQueue<QueryResult>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> anyhow::Result<JoinHandle<()>> {
        let max_concurrent = self.config.max_concurrent;
        let config_arc = self.config.clone();

        // create handlebars
        let mut handlebars = Handlebars::new();
        handlebars.register_helper(
            "json",
            Box::new(
                |h: &handlebars::Helper,
                 _: &Handlebars,
                 _: &handlebars::Context,
                 _: &mut handlebars::RenderContext,
                 out: &mut dyn handlebars::Output|
                 -> handlebars::HelperResult {
                    if let Some(value) = h.param(0) {
                        let json_str = serde_json::to_string(&value.value())
                            .unwrap_or_else(|_| "null".to_string());
                        out.write(&json_str)?;
                    }
                    Ok(())
                },
            ),
        );

        let state = self.state.clone();
        let reaction_id = self.reaction_id.clone();
        let handlebars_arc = Arc::new(handlebars);

        // spawn a background task
        Ok(tokio::spawn(async move {
            let sem = Arc::new(Semaphore::new(max_concurrent as usize));
            let cancel = CancellationToken::new();

            loop {
                tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        info!("[{reaction_id}] Shutdown signal received for reaction_id: {reaction_id}");
                        cancel.cancel();
                        break;
                    }


                    result = priority_queue.dequeue() => {
                        let query_result = result.as_ref();

                        if(query_result.results.is_empty()){
                            // drops the control signals like `bootstrapStarted` or `bootstrapCompleted`
                            debug!("[{reaction_id}] Received empty results for query_id: {}, likely a control signal, skipping processing.", query_result.query_id);
                            continue;

                        } else {
                            tokio::spawn(
                                    Self::process_results(cancel.clone(), query_result.clone(), config_arc.clone(), sem.clone(), handlebars_arc.clone(), state.clone())
                            );
                        }
                    }
                }
            }
        }))
    }

    async fn process_results(
        cancel: CancellationToken,
        query_result: QueryResult,
        config: Arc<ShellReactionConfig>,
        sem: Arc<Semaphore>,
        handlebars: Arc<Handlebars<'static>>,
        state: Arc<ShellReactionState>,
    ) -> anyhow::Result<()> {
        let query_id = query_result.query_id.clone();

        for result in &query_result.results {
            if matches!(result, ResultDiff::Noop) {
                debug!(
                    "[{}] Received Noop result for query_id: {query_id}, skipping processing.",
                    state.reaction_id
                );
                continue;
            }

            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    debug!("[{}] Cancellation requested while processing results for query_id: {query_id}, stopping processing.", state.reaction_id);
                    break;
                }

                result = Self::process_single_result(cancel.clone(), query_id.clone(), result.clone(), config.clone(), sem.clone(), handlebars.clone(), state.clone()) => {
                    if let Err(e) = result {
                        warn!("[{}] Error processing result for query_id: {query_id}. error: {e:?}", state.reaction_id);
                    }
                }

            }
        }
        Ok(())
    }

    async fn process_single_result(
        cancel: CancellationToken,
        query_id: String,
        result: ResultDiff,
        config: Arc<ShellReactionConfig>,
        sem: Arc<Semaphore>,
        handlebars: Arc<Handlebars<'static>>,
        state: Arc<ShellReactionState>,
    ) -> anyhow::Result<()> {
        // get the operation, data val and result type
        let (operation, data_value, result_type) = match result {
            ResultDiff::Add { data } => (OperationType::Add, data, "ADD"),
            ResultDiff::Update { data, .. } => (OperationType::Update, data, "UPDATE"),
            ResultDiff::Delete { data } => (OperationType::Delete, data, "DELETE"),
            ResultDiff::Aggregation { before, after } => {
                let aggregation_data = json!({
                    "before": before,
                    "after": after,
                });
                (OperationType::Update, aggregation_data, "AGGREGATION")
            }
            _ => return Ok(()), // unreachable due to the check in process_results.
        };

        // build the context map for template rendering
        let mut context = Map::new();
        match result_type {
            "ADD" => {
                context.insert("after".to_string(), data_value.clone());
            }
            "UPDATE" | "AGGREGATION" => {
                if let Some(obj) = data_value.as_object() {
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    if let Some(data) = obj.get("data") {
                        context.insert("data".to_string(), data.clone());
                    }
                } else {
                    context.insert("after".to_string(), data_value.clone());
                }
            }
            "DELETE" => {
                context.insert("before".to_string(), data_value.clone());
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "[{}] Unrecognized result type: {result_type}",
                    state.reaction_id
                ));
            } // unreachable due to the enum match in process_single_result
        }

        // insert query metadata into the context
        context.insert(
            "query_name".to_string(),
            Value::String(query_id.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(result_type.to_string()),
        );

        // create the env vars for the command and render the global env values.
        let mut command_env = HashMap::new();
        for (key, value) in &config.env {
            let rendered_value = handlebars.render_template(value, &context)?;
            command_env.insert(key.clone(), rendered_value);
        }

        // get the target template for query and operation
        let spec = config.get_template_spec(&query_id, operation);

        // get the stdin data
        let stdin_data = if let Some(spec) = spec {
            // load and render the local env values if exist.
            for (key, value) in &spec.extension.env {
                let rendered_value = handlebars.render_template(value, &context)?;
                command_env.insert(key.clone(), rendered_value);
            }

            // render the stdin data
            handlebars.render_template(&spec.template, &context)?
        } else {
            // if no template is found, use the raw json string as stdin
            debug!("[{}] No template found for query_id: {query_id}, operation: {operation:?}. Using raw JSON as stdin.", state.reaction_id);
            data_value.to_string()
        };

        // check if the rendered stdin ends with a newline, if not add one to ensure the command receives the data.
        let rendered_stdin = if !stdin_data.ends_with('\n') {
            debug!("[{}] Added newline to rendered stdin for query_id: {query_id}, operation: {operation:?}, rendered_stdin_length: {length}", state.reaction_id, length = stdin_data.len());
            format!("{stdin_data}\n")
        } else {
            stdin_data
        };

        // check if the value of the stdin exceeds the configured limits
        if rendered_stdin.len() > config.max_stdin_bytes {
            // log warn, reject and increment the metric.
            state.metrics.record_stdin_rejection();
            warn!("[{}] Stdin data exceeds max_stdin_bytes limit. query_id: {query_id}, operation: {operation:?}, rendered_stdin_length: {length}, max_stdin_bytes: {max}", state.reaction_id, length = rendered_stdin.len(), max = config.max_stdin_bytes);
            return Err(anyhow::anyhow!("[{}] Stdin data exceeds max_stdin_bytes limit. query_id: {query_id}, operation: {operation:?}, rendered_stdin_length: {length}, max_stdin_bytes: {max}", state.reaction_id, length = rendered_stdin.len(), max = config.max_stdin_bytes));
        }

        // spawn a task to execute the command
        let context = CommandExecutionContext {
            stdin_data: rendered_stdin,
            env: command_env,
            kill_on_drop: config.kill_on_drop,
            timeout_s: config.timeout_s,
            capture_limits: config.capture_limit,
        };

        // get the command to execute for the query_id.
        let command = match config.commands.get(&query_id) {
            Some(cmd) => cmd.clone(),
            None => {
                // this should not happen as the config is validated during reaction creation, but we check again to be safe.
                error!("[{}] No command found for query_id: {query_id} in configuration. This indicates a configuration error. Skipping execution.", state.reaction_id);
                return Err(anyhow::anyhow!("[{}] No command found for query_id: {query_id} in configuration. This indicates a configuration error. Skipping execution.", state.reaction_id));
            }
        };

        // [Blocking] aquire a permit to execute, if the max_concurrent limit is reached, this will wait until a permit is available.
        let permit = tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                debug!("[{}] Cancellation requested while waiting for permit to execute command for query_id: {query_id}, operation: {operation:?}, skipping execution.", state.reaction_id);
                return Ok(());
            }

            permit = sem.clone().acquire_owned() => {
                permit?
            }
        };

        tokio::spawn(async move {
            // ensure the permit is held for the duration of the task
            let _permit = permit;

            // increment the active processes counter
            state.process_state.increment_active_processes();

            // execute the command
            let result = Self::execute_command(
                cancel.clone(),
                command,
                context,
                state.clone(),
                query_id.clone(),
            )
            .await;

            if let Err(e) = result {
                warn!("[{}] Error executing command for query_id: {query_id}, operation: {operation:?}. error: {e:?}", state.reaction_id);
            }

            // decrement the active processes counter
            state.process_state.decrement_active_processes();

            // drop the permit
            drop(_permit);

            // log the number of active processes
            debug!("[{}] Command execution completed for query_id: {query_id}, operation: {operation:?}. Active processes count: {}", state.reaction_id, state.process_state.get_active_processes());
        });

        Ok(())
    }

    async fn execute_command(
        cancel: CancellationToken,
        command: ShellCommand,
        context: CommandExecutionContext,
        state: Arc<ShellReactionState>,
        query_id: String,
    ) -> anyhow::Result<()> {
        // create the main process to be executed.
        let mut cmd = tokio::process::Command::new(command.executable.clone());

        cmd.args(&command.args)
            .envs(context.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(context.kill_on_drop);

        // create a new process group for the child process, this allows to kill the entire group when needed.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))
                    .map_err(std::io::Error::from)?;
                Ok(())
            });
        }

        // spawn the command and handle spwan failures.
        let mut start_timestamp;
        let mut child = match cmd.spawn() {
            Ok(child) => {
                start_timestamp = Utc::now();
                child
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to spawn command. error: {:?}, command: {:?}",
                    state.reaction_id, e, command
                );
                let recent_invocation = RecentInvocation {
                    start_timestamp: Utc::now(),
                    end_timestamp: None,
                    exit_status: Some(-1),
                    stdout: "".to_string(),
                    stderr: format!("[{}] shell-reaction: Failed to spawn command. error: {e:?}, command: {command:?}", state.reaction_id),
                    executable: command.executable.clone(),
                };
                state
                    .recent_invocations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .add_invocation(recent_invocation);
                return Err(anyhow::anyhow!(
                    "[{}] Failed to spawn command. error: {:?}, command: {:?}",
                    state.reaction_id,
                    e,
                    command
                ));
            }
        };

        // get the child pid, returns none if the process has already exited
        let pid = match child.id() {
            Some(pid) => pid,
            None => {
                // This can happen if the process exits very quickly after spawning, before we can get its PID. (the process doesn't need stdin data)
                let output = match child.wait_with_output().await {
                    Ok(output) => output,
                    Err(e) => {
                        warn!("[{}] Failed to wait for child process after spawn failure. error: {:?}, command: {:?}", state.reaction_id, e, command);
                        return Err(anyhow::anyhow!("[{}] Failed to wait for child process after spawn failure. error: {:?}, command: {:?}", state.reaction_id, e, command));
                    }
                };

                let full_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let full_stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let stdout_read =
                    Self::read_limited(output.stdout.as_slice(), context.capture_limits).await;
                let truncated_stdout_data = match stdout_read {
                    Ok((data, truncated)) => {
                        if truncated {
                            state.metrics.record_stdout_truncation();
                            warn!("[{}] Command stdout output exceeded capture limits and was truncated. pid: unknown, command: {:?}, full_stdout_length: {}, capture_limits: {}", state.reaction_id, command, full_stdout.len(), context.capture_limits);
                        }
                        String::from_utf8_lossy(&data).to_string()
                    }
                    Err(e) => {
                        warn!("[{}] Failed to read child stdout. error: {:?}, pid: unknown, command: {:?}", state.reaction_id, e, command);
                        "".to_string()
                    }
                };

                let stderr_read =
                    Self::read_limited(output.stderr.as_slice(), context.capture_limits).await;
                let truncated_stderr_data = match stderr_read {
                    Ok((data, truncated)) => {
                        if truncated {
                            state.metrics.record_stderr_truncation();
                            warn!("[{}] Command stderr output exceeded capture limits and was truncated. pid: unknown, command: {:?}, full_stderr_length: {}, capture_limits: {}", state.reaction_id, command, full_stderr.len(), context.capture_limits);
                        }
                        String::from_utf8_lossy(&data).to_string()
                    }
                    Err(e) => {
                        warn!("[{}] Failed to read child stderr. error: {:?}, pid: unknown, command: {:?}", state.reaction_id, e, command);
                        "".to_string()
                    }
                };

                let exit_status = output.status.code().unwrap_or(-1);
                if exit_status != 0 {
                    state.metrics.record_non_zero_exit();
                    warn!(
                        "[{}] Command exited with non-zero status. exit_status: {}, command: {:?}",
                        state.reaction_id, exit_status, command
                    );
                }

                let recent_invocation = RecentInvocation {
                    start_timestamp,
                    end_timestamp: Some(Utc::now()),
                    exit_status: Some(exit_status),
                    stdout: truncated_stdout_data,
                    stderr: truncated_stderr_data,
                    executable: command.executable.clone(),
                };

                debug!("[{}] Command execution completed for process with unknown PID. exit_status: {}, command: {:?}, full_stdout_length: {}, full_stderr_length: {}, truncated_stdout_length: {}, truncated_stderr_length: {}", state.reaction_id, exit_status, command, full_stdout.len(), full_stderr.len(), recent_invocation.stdout.len(), recent_invocation.stderr.len());

                state
                    .recent_invocations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .add_invocation(recent_invocation);
                return Ok(());
            }
        } as i32;

        // take the child stdin to write input stdin data.
        let child_write = child.stdin.take();
        let child_stdout = child.stdout.take();
        let child_stderr = child.stderr.take();

        // create the write future (stdin delivery)
        let write_fut = async {
            if let Some(mut stdin) = child_write {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(context.stdin_data.as_bytes()).await?;
                stdin.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        };

        // read stdout/stderr to completion
        let capture_limit = context.capture_limits;
        let state_clone_for_read = state.clone();
        let read_fut = async {
            use tokio::io::AsyncReadExt;
            let mut out = Vec::new();
            let mut err = Vec::new();
            tokio::join!(
                async {
                    if let Some(mut s) = child_stdout {
                        let result = Self::read_limited(s, capture_limit).await;
                        match result {
                            Ok((data, truncated)) => {
                                if truncated {
                                    state_clone_for_read.metrics.record_stdout_truncation();
                                    warn!("[{}] Command stdout output exceeded capture limits and was truncated during read. pid: {}, command: {:?}, capture_limits: {}", state_clone_for_read.reaction_id, pid, command, capture_limit);
                                }
                                out = data;
                            }
                            Err(e) => {
                                warn!("[{}] Failed to read child stdout. error: {:?}, pid: {}, command: {:?}", state_clone_for_read.reaction_id, e, pid, command);
                            }
                        }
                    }
                },
                async {
                    if let Some(mut s) = child_stderr {
                        let result = Self::read_limited(s, capture_limit).await;
                        match result {
                            Ok((data, truncated)) => {
                                if truncated {
                                    state_clone_for_read.metrics.record_stderr_truncation();
                                    warn!("[{}] Command stderr output exceeded capture limits and was truncated during read. pid: {}, command: {:?}, capture_limits: {}", state_clone_for_read.reaction_id, pid, command, capture_limit);
                                }
                                err = data;
                            }
                            Err(e) => {
                                warn!("[{}] Failed to read child stderr. error: {:?}, pid: {}, command: {:?}", state_clone_for_read.reaction_id, e, pid, command);
                            }
                        }
                    }
                },
            );
            (out, err)
        };

        // create the wait future
        let wait_fut = async { child.wait().await };

        let timeout_duration = std::time::Duration::from_secs(context.timeout_s);
        let result = tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                // cancellation requested, return None to indicate cancellation.
                None
            }

            result = timeout(
                timeout_duration,
                async {
                    let (write_result, wait_result, (stdout_data, stderr_data)) = tokio::join!(write_fut, wait_fut, read_fut);
                    (write_result, wait_result, stdout_data, stderr_data)
                }
            ) => {
                Some(result) // Some(Err(e)) indicates timeout, Some(Ok((write_result, wait_result, stdout_data, stderr_data))) indicates command completed (successfully or with error))
            }

        };

        match result {
            None => {
                // cancellation case, kill the process group and return.
                debug!("[{}] Command execution cancelled. Killing process group. pid: {}, command: {:?}", state.reaction_id, pid, command);
                Self::terminate_process_group(&mut child, pid, &state.reaction_id, &state).await;
            }
            Some(Err(e)) => {
                // timeout case, kill the process group and return.
                warn!("[{}] Command execution timed out after {} seconds. Killing process group. pid: {}, command: {:?}", state.reaction_id, context.timeout_s, pid, command);
                state.metrics.record_timeout();
                Self::terminate_process_group(&mut child, pid, &state.reaction_id, &state).await;
            }
            Some(Ok((write_result, wait_result, stdout_data, stderr_data))) => {
                if let Err(e) = write_result {
                    warn!(
                        "[{}] Failed to write to child stdin. error: {:?}, command: {:?}",
                        state.reaction_id, e, command
                    );
                }

                let exit_status = match wait_result {
                    Ok(status) => {
                        let exit_status = status.code().unwrap_or(-1);
                        // record the non-zero exit status
                        if exit_status != 0 {
                            state.metrics.record_non_zero_exit();
                            warn!("[{}] Command exited with non-zero status. exit_status: {}, pid: {}, command: {:?}", state.reaction_id, exit_status, pid, command);
                        }
                        exit_status
                    }
                    Err(e) => {
                        state.metrics.record_non_zero_exit(); // if we fail to wait for the process, we consider it a failure and record it as a non-zero exit for metrics purposes.
                        warn!("[{}] Failed to wait for child process. error: {:?}, pid: {}, command: {:?}", state.reaction_id, e, pid, command);
                        -1
                    }
                };

                let full_stdout = String::from_utf8_lossy(&stdout_data).to_string();
                let full_stderr = String::from_utf8_lossy(&stderr_data).to_string();

                let recent_invocation = RecentInvocation {
                    start_timestamp,
                    end_timestamp: Some(Utc::now()),
                    exit_status: Some(exit_status),
                    stdout: full_stdout,
                    stderr: full_stderr,
                    executable: command.executable.clone(),
                };

                state.metrics.record_result_processed();
                debug!("[{}] Command execution completed. pid: {}, exit_status: {}, command: {:?}, stdout_length: {}, stderr_length: {}", state.reaction_id, pid, exit_status, command, recent_invocation.stdout.len(), recent_invocation.stderr.len());

                state
                    .recent_invocations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .add_invocation(recent_invocation);
            }
        }
        Ok(())
    }

    async fn terminate_process_group(
        child: &mut Child,
        pid: i32,
        reaction_id: &str,
        state: &Arc<ShellReactionState>,
    ) {
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }

        match timeout(std::time::Duration::from_millis(750), child.wait()).await {
            Ok(Ok(status)) => {
                debug!("[{}] Process group terminated gracefully with SIGTERM. pid: {}, exit_status: {:?}", state.reaction_id, pid, status.code());
            }
            _ => {
                // if the process didn't exit, send SIGKILL to the whole process group
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }

                let _ = child.wait().await;
                debug!(
                    "[{}] Process group killed with SIGKILL after failed SIGTERM. pid: {}",
                    state.reaction_id, pid
                );
            }
        }
    }

    pub async fn read_limited<R>(mut reader: R, mut limit: usize) -> anyhow::Result<(Vec<u8>, bool)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::with_capacity(limit);
        let mut temp = [0u8; 4096];
        let mut truncated = false;
        loop {
            if limit == 0 {
                break;
            }

            let n = reader.read(&mut temp).await?;

            if n == 0 {
                break;
            }

            if n <= limit {
                buf.extend_from_slice(&temp[..n]);
                limit -= n;
            } else {
                buf.extend_from_slice(&temp[..limit]);
                truncated = true;
                break;
            }
        }
        Ok((buf, truncated))
    }
}
