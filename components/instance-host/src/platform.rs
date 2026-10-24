// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{io, os::fd::AsRawFd, process::Child};

use anyhow::{ensure, Context, Result};

pub(crate) fn isolate_process_group() -> Result<()> {
    // Only the host executable opts in, before any graph or plugin is loaded.
    let pid = unsafe { libc::getpid() };
    if unsafe { libc::getpgrp() } != pid {
        ensure!(
            unsafe { libc::setpgid(0, 0) } == 0,
            "create instance process group: {}",
            io::Error::last_os_error()
        );
    }
    Ok(())
}

pub(crate) fn kill_owned(child: &mut Child) -> Result<()> {
    let pid = i32::try_from(child.id()).context("child PID overflow")?;
    // Command::process_group(0) made this exact child's PID its group ID.
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub(crate) fn kill_instance() -> ! {
    // The CLI checked group ownership before installing this watchdog.
    unsafe { libc::kill(0, libc::SIGKILL) };
    std::process::exit(137);
}

pub(crate) fn nonblocking(file: &impl AsRawFd) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "read pipe flags: {}",
        io::Error::last_os_error()
    );
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0,
        "set pipe flags: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

pub(crate) fn lock(file: &std::fs::File) -> Result<()> {
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "state directory already has an owner: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

pub(crate) fn owner_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub(crate) fn process_absent(pid: u32) -> Result<bool> {
    let pid = i32::try_from(pid).context("process PID overflow")?;
    ensure!(pid > 0, "invalid process identity");
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(false);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(true);
    }
    Err(error.into())
}
