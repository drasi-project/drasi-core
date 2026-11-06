// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Experimental ownership protocol, not a production plugin ABI.

use std::ffi::c_void;

pub const OK: u32 = 0;
pub const INVALID: u32 = 1;
pub const FULL: u32 = 2;
pub const CLOSED: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Token {
    pub owner: [u8; 16],
    pub lease: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Host {
    pub context: *const c_void,
    pub retain_context: unsafe extern "C" fn(*const c_void),
    pub release_context: unsafe extern "C" fn(*const c_void),
    pub retain: unsafe extern "C" fn(*const c_void, Token, *mut Token) -> u32,
    pub release: unsafe extern "C" fn(*const c_void, Token) -> u32,
}

#[repr(C)]
pub struct Reply {
    pub owner: *mut c_void,
    pub data: *const u8,
    pub len: usize,
    pub token: Token,
    pub release: unsafe extern "C" fn(*mut c_void),
}

pub type Create = unsafe extern "C" fn(Host, Token, u64, *mut *mut c_void) -> u32;
pub type CloneInput = unsafe extern "C" fn(*const c_void, *mut *mut c_void) -> u32;
pub type Emit = unsafe extern "C" fn(*const c_void, *mut Reply) -> u32;
pub type Destroy = unsafe extern "C" fn(*mut c_void);
