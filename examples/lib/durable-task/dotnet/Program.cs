// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using DurableTask.Core;

internal static class Walkthrough
{
    public const string OrderId = "order-42";

    public static async Task<string?> RunAsync(DemoHost host, DemoConsole console)
    {
        host.Database.CommandOutput = console.Output;
        await console.Output.WriteLineAsync("\nFollow one order: placed -> paid -> shipped -> delivered.");
        await console.Output.WriteLineAsync("PostgreSQL -> Drasi query/reaction -> DTS -> OrderWorkflow");
        await console.Output.WriteLineAsync("\nThe query watches each order's status:");
        await console.Output.WriteLineAsync(await File.ReadAllTextAsync(
            Path.Combine(host.Root, "query.cypher"), host.Cancellation));

        string[] statuses = ["placed", "paid", "shipped"];
        for (int step = 0; step < statuses.Length; step++)
        {
            string status = statuses[step];
            if (!await console.NextStepAsync($"{step + 1}/4. Mark the order as {status}.")) return null;
            if (step == 0)
                await host.Database.PlaceOrderAsync(OrderId);
            else
                await host.Database.UpdateStatusAsync(OrderId, status);
            OrchestrationState workflow = await host.WaitForStatusAsync(OrderId, status);
            await console.ShowResultAsync(host.Database, OrderId, step == 0 ? "ADDED" : "UPDATED", workflow);
        }

        if (!await console.NextStepAsync("4/4. Mark the order as delivered. The workflow should complete.")) return null;
        await host.Database.UpdateStatusAsync(OrderId, "delivered");
        OrchestrationState completed = await host.StateWhenAsync(OrderId,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        string outcome = DemoHost.Read<string>(completed.Output);
        await console.ShowResultAsync(host.Database, OrderId, "UPDATED", completed);
        await console.Output.WriteLineAsync(outcome);
        await console.Output.WriteLineAsync("\nEvery status change reached the workflow through Drasi. The delivered order stays in the query.");
        return outcome;
    }
}
