-- Copyright 2025 The Drasi Authors.
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- You may obtain a copy of the License at
--
--     http://www.apache.org/licenses/LICENSE-2.0
--
-- Unless required by applicable law or agreed to in writing, software
-- distributed under the License is distributed on an "AS IS" BASIS,
-- WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
-- See the License for the specific language governing permissions and
-- limitations under the License.

-- ============================================================================
-- Orders schema (relational, mirroring the state-machine example entities)
-- ============================================================================
-- An order progresses through six stages, each represented by real relational
-- data — mirroring the original example's Order / Payment / StockPick / Shipment
-- entities:
--
--   NEW        an order row with is_draft = 1
--   CONFIRMED  the order row's is_draft flipped to 0
--   PAID       a settled `payments` row      (was: (o:Order)-[:PAID_BY]->(p:Payment))
--   PICKED     a `stock_picks` row           (was: (o:Order)-[:RESERVED]->(:StockPick))
--   SHIPPED    a `shipments` row             (was: (o:Order)-[:DISPATCHED]->(:Shipment))
--   DELIVERED  the shipment row's delivered flag is 1
--
-- The Drasi Postgres source maps each table to graph nodes labelled by the table
-- name. Rather than join across nodes (which would require graph relationships),
-- each stage query returns the owning `order_id`, and the state machine correlates
-- the stages into a per-order lifecycle by that key. Advancement is therefore
-- mostly INSERT-driven (settled payments, stock picks, shipments), which the
-- source delivers as clean node additions.
--
-- NOTE: `is_draft` and `delivered` are modelled as SMALLINT flags (0/1) rather
-- than BOOLEAN. The Postgres CDC source currently mis-decodes streamed BOOLEAN
-- values (they always decode as true), so a boolean flag flip would not be
-- observed on the change stream. Integers stream correctly. See the tracking
-- issue linked from the example README.

CREATE TABLE IF NOT EXISTS orders (
    id            TEXT PRIMARY KEY,
    customer      TEXT NOT NULL,
    amount        DOUBLE PRECISION NOT NULL DEFAULT 0,
    is_draft      SMALLINT NOT NULL DEFAULT 1,
    next_stage_at TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS payments (
    id         TEXT PRIMARY KEY,
    order_id   TEXT NOT NULL REFERENCES orders(id),
    status     TEXT NOT NULL DEFAULT 'settled',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS stock_picks (
    id         TEXT PRIMARY KEY,
    order_id   TEXT NOT NULL REFERENCES orders(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS shipments (
    id         TEXT PRIMARY KEY,
    order_id   TEXT NOT NULL REFERENCES orders(id),
    delivered  SMALLINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Logical replication needs full row images for UPDATEs (is_draft, delivered).
ALTER TABLE orders      REPLICA IDENTITY FULL;
ALTER TABLE payments    REPLICA IDENTITY FULL;
ALTER TABLE stock_picks REPLICA IDENTITY FULL;
ALTER TABLE shipments   REPLICA IDENTITY FULL;

-- ============================================================================
-- Advancement procedures
-- ============================================================================

-- Schedule an order to advance to its next stage after a short delay. Called by
-- the stored-procedure reaction whenever an order enters a stage. No-op once the
-- order has been delivered.
CREATE OR REPLACE PROCEDURE schedule_advance(p_order_id TEXT)
LANGUAGE sql AS $$
    UPDATE orders o
       SET next_stage_at = now() + INTERVAL '1 second'
     WHERE o.id = p_order_id
       AND o.next_stage_at IS NULL
       AND NOT EXISTS (
           SELECT 1 FROM shipments s WHERE s.order_id = o.id AND s.delivered = 1
       );
$$;

-- Apply due advancements. For each order whose next_stage_at has elapsed, perform
-- the single mutation that moves it to the next stage, then clear the schedule.
-- Returns the number of orders advanced. Called by the example's pacing driver.
-- Apply one due advancement per call. Advancing a single order per call (and
-- touching its row at most once) keeps each CDC transaction to a single change,
-- which the change stream and continuous queries handle reliably. The pacing
-- driver calls this frequently (see the example's 1 Hz loop). Returns 1 if an
-- order was advanced, else 0.
CREATE OR REPLACE FUNCTION advance_due_orders()
RETURNS INTEGER
LANGUAGE plpgsql AS $$
DECLARE
    r       RECORD;
    n_count INTEGER := 0;
BEGIN
    FOR r IN
        SELECT * FROM orders
         WHERE next_stage_at IS NOT NULL AND next_stage_at <= now()
         ORDER BY next_stage_at
         LIMIT 1
         FOR UPDATE SKIP LOCKED
    LOOP
        -- Each branch performs the single mutation for the next stage and clears
        -- the schedule. The orders row is touched at most once per advancement so
        -- the CDC stream carries one clean change per stage (no collapsed
        -- double-updates).
        IF r.is_draft = 1 THEN
            -- NEW -> CONFIRMED
            UPDATE orders SET is_draft = 0, next_stage_at = NULL WHERE id = r.id;
        ELSIF NOT EXISTS (SELECT 1 FROM payments p WHERE p.order_id = r.id) THEN
            -- CONFIRMED -> PAID
            INSERT INTO payments (id, order_id, status)
            VALUES (r.id || '-pay', r.id, 'settled');
            UPDATE orders SET next_stage_at = NULL WHERE id = r.id;
        ELSIF NOT EXISTS (SELECT 1 FROM stock_picks sp WHERE sp.order_id = r.id) THEN
            -- PAID -> PICKED
            INSERT INTO stock_picks (id, order_id) VALUES (r.id || '-pick', r.id);
            UPDATE orders SET next_stage_at = NULL WHERE id = r.id;
        ELSIF NOT EXISTS (SELECT 1 FROM shipments s WHERE s.order_id = r.id) THEN
            -- PICKED -> SHIPPED
            INSERT INTO shipments (id, order_id, delivered) VALUES (r.id || '-ship', r.id, 0);
            UPDATE orders SET next_stage_at = NULL WHERE id = r.id;
        ELSIF NOT EXISTS (
            SELECT 1 FROM shipments s WHERE s.order_id = r.id AND s.delivered = 1
        ) THEN
            -- SHIPPED -> DELIVERED
            UPDATE shipments SET delivered = 1 WHERE order_id = r.id;
            UPDATE orders SET next_stage_at = NULL WHERE id = r.id;
        ELSE
            UPDATE orders SET next_stage_at = NULL WHERE id = r.id;
        END IF;

        n_count := n_count + 1;
    END LOOP;
    RETURN n_count;
END;
$$;

-- Insert a new draft order (used by the example's seeder).
CREATE OR REPLACE PROCEDURE new_order(p_order_id TEXT, p_customer TEXT, p_amount DOUBLE PRECISION)
LANGUAGE sql AS $$
    INSERT INTO orders (id, customer, amount, is_draft)
    VALUES (p_order_id, p_customer, p_amount, 1)
    ON CONFLICT (id) DO NOTHING;
$$;

-- ============================================================================
-- Logical replication publication
-- ============================================================================
-- The Drasi Postgres source auto-creates its replication slot; do NOT pre-create
-- the slot here (doing so replays seed rows on top of the bootstrap snapshot and
-- doubles the data). The publication, however, must exist and cover every table.
CREATE PUBLICATION drasi_pub FOR TABLE orders, payments, stock_picks, shipments;

-- No seed data: the example's seeder inserts orders live so they animate through
-- the full lifecycle. (Rows that exist before startup arrive via bootstrap, which
-- reactions do not observe, so they would not advance.)
