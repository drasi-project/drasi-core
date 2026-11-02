-- Copyright 2026 The Drasi Authors.
-- SPDX-License-Identifier: Apache-2.0

CREATE TABLE orders (
    id TEXT PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('placed', 'paid', 'shipped', 'delivered'))
);
ALTER TABLE orders REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_temporal FOR TABLE orders;
