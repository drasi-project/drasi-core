-- Copyright 2026 The Drasi Authors.
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE inventory (
    product TEXT PRIMARY KEY,
    stock INTEGER NOT NULL CHECK (stock >= 0)
);
ALTER TABLE inventory REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_temporal FOR TABLE inventory;
