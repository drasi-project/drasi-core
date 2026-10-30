-- Copyright 2026 The Drasi Authors.
-- Licensed under the Apache License, Version 2.0.

CREATE TABLE public.orders (
    order_id text PRIMARY KEY,
    status text NOT NULL DEFAULT 'placed'
);

ALTER TABLE public.orders REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_orders FOR TABLE public.orders;
