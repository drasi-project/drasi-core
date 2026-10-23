// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using DurableTask.Core;

internal sealed class DemoConsole(TextReader input, TextWriter output, CancellationToken cancellation)
{
    public TextWriter Output => output;

    public async Task<bool> NextStepAsync(string description)
    {
        await output.WriteLineAsync($"\n{description}");
        while (true)
        {
            await output.WriteLineAsync("Press Enter to continue, or q to quit.");
            string? reply = await input.ReadLineAsync(cancellation);
            if (reply is null || reply.Trim().Equals("q", StringComparison.OrdinalIgnoreCase)) return false;
            if (string.IsNullOrWhiteSpace(reply)) return true;
            await output.WriteLineAsync("No change made. Use Enter or q.");
        }
    }

    public async Task ShowResultAsync(OrdersDatabase database, string orderId, string operation, OrchestrationState workflow)
    {
        OrderRow order = await database.ReadOrderAsync(orderId)
            ?? throw new InvalidOperationException("The example order unexpectedly disappeared");
        await output.WriteLineAsync($"\nOrder: {order.OrderId}");
        await output.WriteLineAsync($"Status: {order.Status}");
        await output.WriteLineAsync($"Query change: {operation}");
        await output.WriteLineAsync($"Workflow: {workflow.OrchestrationStatus.ToString().ToUpperInvariant()}");
    }
}
