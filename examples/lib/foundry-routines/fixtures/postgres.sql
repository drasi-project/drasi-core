CREATE ROLE drasi WITH LOGIN REPLICATION PASSWORD 'local-fixture-only';
CREATE TABLE orders (
    order_id TEXT PRIMARY KEY,
    sku TEXT NOT NULL,
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    status TEXT NOT NULL,
    notes TEXT NOT NULL DEFAULT ''
);
INSERT INTO orders VALUES
('order-1001', 'SKU-A', 5, 'pending', ''),
('order-1002', 'SKU-B', 3, 'pending', '');
ALTER TABLE orders REPLICA IDENTITY FULL;
GRANT CONNECT ON DATABASE orders TO drasi;
GRANT USAGE ON SCHEMA public TO drasi;
GRANT SELECT ON orders TO drasi;
CREATE PUBLICATION foundry_demo FOR TABLE orders;
