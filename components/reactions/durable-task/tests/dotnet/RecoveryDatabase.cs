// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

internal sealed partial class OrdersDatabase
{
    public Task InsertOrderAsync(string episode, long revision, string reason) =>
        WriteAsync("INSERT INTO public.recovery_orders (episode_id, revision, reason, needs_attention) VALUES ($1, $2, $3, true)",
            episode, revision, reason);

    public Task UpdateOrderAsync(string episode, long revision, string reason, bool needsAttention = true) =>
        WriteAsync("UPDATE public.recovery_orders SET revision = $2, reason = $3, needs_attention = $4 WHERE episode_id = $1",
            episode, revision, reason, needsAttention);

    public Task DeleteOrderAsync(string episode) =>
        WriteAsync("DELETE FROM public.recovery_orders WHERE episode_id = $1", episode);
}
