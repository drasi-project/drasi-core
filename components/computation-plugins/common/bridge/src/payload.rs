// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Allocation-free structural checks before the standard bincode validators run.
//! These are additional transport bounds, not replacements for those validators.

use crate::{BridgeError, Limits, Namespace, PortableSchema, Result};

pub(crate) struct PayloadGuard<'a> {
    limits: &'a Limits,
    namespace: &'a Namespace,
    values: usize,
}

impl<'a> PayloadGuard<'a> {
    pub(crate) fn new(limits: &'a Limits, namespace: &'a Namespace) -> Self {
        Self {
            limits,
            namespace,
            values: 0,
        }
    }

    pub(crate) fn identity(
        &mut self,
        schema: PortableSchema,
        namespace: &str,
        bytes: &[u8],
    ) -> Result<()> {
        if namespace.is_empty() || namespace.len() > 1024 || bytes.is_empty() || bytes.len() > 4096
        {
            return Err(BridgeError::invalid(
                "record identity exceeds bridge bounds",
            ));
        }
        match schema {
            PortableSchema::GraphChange => {
                let mut reader = Reader::new(bytes, self);
                reader.reference()?;
                reader.finish()
            }
            PortableSchema::QueryChange => {
                let encoded = namespace
                    .strip_prefix("drasi.query-row/")
                    .ok_or_else(|| BridgeError::invalid("invalid query identity namespace"))?;
                if encoded.len() > 512 || encoded.len() % 2 != 0 || !encoded.is_ascii() {
                    return Err(BridgeError::invalid("invalid query identity encoding"));
                }
                let decoded = (0..encoded.len())
                    .step_by(2)
                    .map(|offset| {
                        u8::from_str_radix(&encoded[offset..offset + 2], 16)
                            .map_err(|_| BridgeError::invalid("invalid query identity encoding"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                self.namespace.query(
                    std::str::from_utf8(&decoded)
                        .map_err(|_| BridgeError::invalid("query identity is not UTF-8"))?,
                )
            }
            PortableSchema::SourceTransaction => self.namespace.source(namespace),
        }
    }

    pub(crate) fn payload(&mut self, schema: PortableSchema, bytes: &[u8]) -> Result<()> {
        let mut reader = Reader::new(bytes, self);
        match schema {
            PortableSchema::GraphChange => reader.change(0)?,
            PortableSchema::QueryChange => reader.row()?,
            PortableSchema::SourceTransaction => {
                if reader.u32()? != 1 {
                    return Err(BridgeError::invalid(
                        "unsupported complete transaction version",
                    ));
                }
                let source = reader.string(256)?;
                reader.guard.namespace.source(source)?;
                if reader.blob(256)?.is_empty() || reader.blob(16 * 1024)?.is_empty() {
                    return Err(BridgeError::invalid(
                        "empty transaction identity or commit position",
                    ));
                }
                let count = reader.count()?;
                if count > reader.guard.limits.max_operations {
                    return Err(BridgeError::invalid(
                        "too many complete transaction changes",
                    ));
                }
                for _ in 0..count {
                    reader.change(0)?;
                }
            }
        }
        reader.finish()
    }
}

struct Reader<'b, 'g, 'a> {
    input: &'b [u8],
    guard: &'g mut PayloadGuard<'a>,
}

impl<'b, 'g, 'a> Reader<'b, 'g, 'a> {
    fn new(input: &'b [u8], guard: &'g mut PayloadGuard<'a>) -> Self {
        Self { input, guard }
    }

    fn finish(self) -> Result<()> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(BridgeError::invalid("trailing typed-record bytes"))
        }
    }

    fn take(&mut self, count: usize) -> Result<&'b [u8]> {
        if count > self.input.len() {
            return Err(BridgeError::invalid("truncated typed record"));
        }
        let (value, rest) = self.input.split_at(count);
        self.input = rest;
        Ok(value)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn boolean(&mut self) -> Result<bool> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(BridgeError::invalid(
                "invalid typed-record boolean or option",
            )),
        }
    }

    fn work(&mut self, depth: usize) -> Result<()> {
        self.guard.values += 1;
        if depth > self.guard.limits.max_depth || self.guard.values > self.guard.limits.max_values {
            return Err(BridgeError::invalid(
                "typed record depth or value budget exceeded",
            ));
        }
        Ok(())
    }

    fn count(&mut self) -> Result<usize> {
        let count = usize::try_from(self.u64()?)
            .map_err(|_| BridgeError::invalid("typed collection size overflow"))?;
        if count > self.guard.limits.max_values || count > self.input.len() {
            return Err(BridgeError::invalid(
                "typed collection exceeds remaining input or limits",
            ));
        }
        Ok(count)
    }

    fn blob(&mut self, max: usize) -> Result<&'b [u8]> {
        let length = usize::try_from(self.u64()?)
            .map_err(|_| BridgeError::invalid("typed byte length overflow"))?;
        if length > max {
            return Err(BridgeError::invalid(
                "typed byte field exceeds bridge limit",
            ));
        }
        self.take(length)
    }

    fn string(&mut self, max: usize) -> Result<&'b str> {
        self.work(0)?;
        std::str::from_utf8(self.blob(max)?)
            .map_err(|_| BridgeError::invalid("typed string is not UTF-8"))
    }

    fn reference(&mut self) -> Result<()> {
        let source = self.string(256)?;
        self.guard.namespace.source(source)?;
        let id = self.string(256)?;
        crate::config::identifier(id)
    }

    fn metadata(&mut self) -> Result<()> {
        self.reference()?;
        for _ in 0..self.count()? {
            self.string(256)?;
        }
        self.take(8)?;
        Ok(())
    }

    fn element(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        let kind = self.u32()?;
        self.metadata()?;
        match kind {
            0 => {}
            1 => {
                self.reference()?;
                self.reference()?;
            }
            _ => return Err(BridgeError::invalid("unknown graph element kind")),
        }
        self.properties(depth + 1)
    }

    fn change(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        match self.u32()? {
            0 | 1 => self.element(depth + 1),
            2 => self.metadata(),
            _ => Err(BridgeError::invalid(
                "scheduled notifications and unknown source changes are not portable bridge data",
            )),
        }
    }

    fn properties(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        for _ in 0..self.count()? {
            self.string(256)?;
            self.element_value(depth + 1)?;
        }
        Ok(())
    }

    fn element_value(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        match self.u32()? {
            0 => {}
            1 => {
                self.boolean()?;
            }
            2 | 3 => {
                self.take(8)?;
            }
            4 => {
                self.string(64 * 1024)?;
            }
            5 => {
                for _ in 0..self.count()? {
                    self.element_value(depth + 1)?;
                }
            }
            6 => self.properties(depth + 1)?,
            7 | 8 => {
                self.string(256)?;
            }
            _ => return Err(BridgeError::invalid("unknown graph value kind")),
        }
        Ok(())
    }

    fn row(&mut self) -> Result<()> {
        let query = self.string(256)?;
        self.guard.namespace.query(query)?;
        self.take(8)?;
        self.query_values(0)?;
        match self.u32()? {
            0 => {}
            1 => {
                for _ in 0..self.count()? {
                    self.string(256)?;
                }
                self.boolean()?;
                self.boolean()?;
            }
            _ => return Err(BridgeError::invalid("unknown query row kind")),
        }
        Ok(())
    }

    fn query_values(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        for _ in 0..self.count()? {
            self.string(256)?;
            self.query_value(depth + 1)?;
        }
        Ok(())
    }

    fn query_value(&mut self, depth: usize) -> Result<()> {
        self.work(depth)?;
        match self.u32()? {
            0 => {}
            1 => {
                self.boolean()?;
            }
            2..=4 => {
                self.take(8)?;
            }
            5 => {
                self.string(64 * 1024)?;
            }
            6 => {
                for _ in 0..self.count()? {
                    self.query_value(depth + 1)?;
                }
            }
            7 => self.query_values(depth + 1)?,
            8 | 9 | 11 => {
                self.string(256)?;
            }
            10 => {
                self.string(256)?;
                self.take(4)?;
            }
            12 => {
                self.string(256)?;
                if self.boolean()? {
                    self.string(256)?;
                }
            }
            13 => {
                self.take(28)?;
            }
            14 => {
                for _ in 0..2 {
                    if self.boolean()? {
                        self.take(8)?;
                    }
                }
            }
            15 => self.element(depth + 1)?,
            16 => self.metadata()?,
            17 => self.reference()?,
            _ => return Err(BridgeError::invalid(
                "executable expressions, awaiting values and unknown query values are not portable",
            )),
        }
        Ok(())
    }
}

/// JSON annotations remain bytes, but known framework readers later decode
/// them. Check nesting without first constructing a recursively dropped Value.
pub(crate) fn annotation_json(bytes: &[u8], max_depth: usize) -> Result<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > max_depth {
                        return Err(BridgeError::invalid("annotation JSON nesting limit"));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}
