// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(clippy::missing_safety_doc)]

#[path = "../../src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

use protocol::{Host, Reply, Token, CLOSED, FULL, INVALID, OK};
use std::{ffi::c_void, ptr};

struct HeldInput {
    ownership: Option<(Host, Token)>,
    value: u64,
}

impl HeldInput {
    unsafe fn acquire(host: Host, input: Token, value: u64) -> Result<Self, u32> {
        if input == Token::default() {
            return Ok(Self {
                ownership: None,
                value,
            });
        }
        let mut token = Token::default();
        let status = unsafe { (host.retain)(host.context, input, &mut token) };
        if status == FULL || status == CLOSED {
            return Ok(Self {
                ownership: None,
                value,
            });
        }
        if status != OK {
            return Err(status);
        }
        unsafe { (host.retain_context)(host.context) };
        Ok(Self {
            ownership: Some((host, token)),
            value,
        })
    }

    unsafe fn copy(&self, value: u64) -> Result<Self, u32> {
        match self.ownership {
            Some((host, token)) => unsafe { Self::acquire(host, token, value) },
            None => Ok(Self {
                ownership: None,
                value,
            }),
        }
    }
}

impl Drop for HeldInput {
    fn drop(&mut self) {
        if let Some((host, token)) = self.ownership {
            let status = unsafe { (host.release)(host.context, token) };
            if status != OK {
                eprintln!("Native timing fixture release failed: {status}");
            }
            unsafe { (host.release_context)(host.context) };
        }
    }
}

struct Buffer {
    input: HeldInput,
    bytes: [u8; 8],
}

#[no_mangle]
pub unsafe extern "C" fn timing_create(
    host: Host,
    token: Token,
    value: u64,
    out: *mut *mut c_void,
) -> u32 {
    if out.is_null() || host.context.is_null() {
        return INVALID;
    }
    unsafe { out.write(ptr::null_mut()) };
    match unsafe { HeldInput::acquire(host, token, value) } {
        Ok(input) => {
            unsafe { out.write(Box::into_raw(Box::new(input)).cast()) };
            OK
        }
        Err(status) => status,
    }
}

#[no_mangle]
pub unsafe extern "C" fn timing_clone(input: *const c_void, out: *mut *mut c_void) -> u32 {
    if input.is_null() || out.is_null() {
        return INVALID;
    }
    unsafe { out.write(ptr::null_mut()) };
    let input = unsafe { &*input.cast::<HeldInput>() };
    match unsafe { input.copy(input.value) } {
        Ok(copy) => {
            unsafe { out.write(Box::into_raw(Box::new(copy)).cast()) };
            OK
        }
        Err(status) => status,
    }
}

#[no_mangle]
pub unsafe extern "C" fn timing_emit(input: *const c_void, out: *mut Reply) -> u32 {
    if input.is_null() || out.is_null() {
        return INVALID;
    }
    let input = unsafe { &*input.cast::<HeldInput>() };
    let Some(value) = input.value.checked_add(1) else {
        return INVALID;
    };
    let owned = match unsafe { input.copy(value) } {
        Ok(owned) => owned,
        Err(status) => return status,
    };
    let buffer = Box::new(Buffer {
        input: owned,
        bytes: value.to_le_bytes(),
    });
    let reply = Reply {
        owner: ptr::null_mut(),
        data: buffer.bytes.as_ptr(),
        len: buffer.bytes.len(),
        token: buffer
            .input
            .ownership
            .map(|(_, token)| token)
            .unwrap_or_default(),
        release: release_buffer,
    };
    unsafe {
        out.write(Reply {
            owner: Box::into_raw(buffer).cast(),
            ..reply
        })
    };
    OK
}

#[no_mangle]
pub unsafe extern "C" fn timing_destroy(input: *mut c_void) {
    if !input.is_null() {
        unsafe { drop(Box::from_raw(input.cast::<HeldInput>())) };
    }
}

unsafe extern "C" fn release_buffer(buffer: *mut c_void) {
    if !buffer.is_null() {
        unsafe { drop(Box::from_raw(buffer.cast::<Buffer>())) };
    }
}
