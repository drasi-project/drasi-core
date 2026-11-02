-- Copyright 2026 The Drasi Authors.
-- Licensed under the Apache License, Version 2.0.

CREATE TABLE public.recovery_orders (
    episode_id text PRIMARY KEY,
    revision bigint NOT NULL,
    reason text NOT NULL,
    needs_attention boolean NOT NULL DEFAULT true
);

ALTER TABLE public.recovery_orders REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_recovery_orders FOR TABLE public.recovery_orders;
