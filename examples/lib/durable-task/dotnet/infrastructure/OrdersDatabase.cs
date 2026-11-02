// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Diagnostics;
using System.Text.Json;
using Npgsql;

internal sealed partial class OrdersDatabase(string connectionString, CancellationToken cancellation) : IAsyncDisposable
{
    readonly NpgsqlDataSource dataSource = NpgsqlDataSource.Create(connectionString);
    readonly List<object> writes = [];
    public IReadOnlyList<object> Writes => writes;
    public TextWriter? CommandOutput { get; set; }

    public async Task InitializeAsync(string schema)
    {
        Stopwatch elapsed = Stopwatch.StartNew();
        while (true)
        {
            try
            {
                await using NpgsqlConnection connection = await dataSource.OpenConnectionAsync(cancellation);
                break;
            }
            catch (NpgsqlException error) when (error.IsTransient && elapsed.Elapsed < TimeSpan.FromSeconds(60))
            {
                await Task.Delay(250, cancellation);
            }
        }
        await using NpgsqlCommand command = dataSource.CreateCommand(schema);
        await command.ExecuteNonQueryAsync(cancellation);
    }

    public Task PlaceOrderAsync(string orderId) =>
        WriteAsync("INSERT INTO public.orders (order_id, status) VALUES ($1, $2)", orderId, "placed");

    public Task UpdateStatusAsync(string orderId, string status) =>
        WriteAsync("UPDATE public.orders SET status = $2 WHERE order_id = $1", orderId, status);

    public async Task<OrderRow?> ReadOrderAsync(string orderId)
    {
        await using NpgsqlCommand command = dataSource.CreateCommand(
            "SELECT order_id, status FROM public.orders WHERE order_id = $1");
        command.Parameters.AddWithValue(orderId);
        await using NpgsqlDataReader reader = await command.ExecuteReaderAsync(cancellation);
        return await reader.ReadAsync(cancellation)
            ? new OrderRow(reader.GetString(0), reader.GetString(1))
            : null;
    }

    async Task WriteAsync(string sql, params object[] parameters)
    {
        if (CommandOutput is { } output)
        {
            await output.WriteLineAsync($"SQL: {sql}");
            await output.WriteLineAsync("  " + string.Join(", ", parameters.Select(
                (value, index) => $"${index + 1} = {JsonSerializer.Serialize(value)}")));
        }
        await using NpgsqlCommand command = dataSource.CreateCommand(sql);
        foreach (object value in parameters) command.Parameters.Add(new NpgsqlParameter { Value = value });
        int affectedRows = await command.ExecuteNonQueryAsync(cancellation);
        writes.Add(new { sql, parameters, affectedRows });
        if (affectedRows != 1)
            throw new InvalidOperationException($"Expected one affected order, got {affectedRows}");
    }

    public async Task WaitForReplicationAsync(string slot)
    {
        await using NpgsqlCommand command = dataSource.CreateCommand(
            "SELECT active FROM pg_replication_slots WHERE slot_name = $1");
        command.Parameters.AddWithValue(slot);
        Stopwatch elapsed = Stopwatch.StartNew();
        while (elapsed.Elapsed < TimeSpan.FromSeconds(60))
        {
            if (await command.ExecuteScalarAsync(cancellation) is true) return;
            await Task.Delay(100, cancellation);
        }
        throw new TimeoutException($"PostgreSQL replication slot {slot} did not become active; inspect drasi.log");
    }

    public ValueTask DisposeAsync() => dataSource.DisposeAsync();
}

internal sealed record OrderRow(string OrderId, string Status);
