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
        await console.Output.WriteLineAsync($"\nDTS dashboard: {host.DashboardUrl}");
        await console.Output.WriteLineAsync($"Open it in your browser. Select the default task hub, then find {OrderId} after the first step.");
        await console.Output.WriteLineAsync("Open History, then refresh after each step to see the named events and activities.");
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
        await console.NextStepAsync(
            $"Inspect the completed workflow's history in the dashboard: {host.DashboardUrl}",
            "Press Enter or q to stop the demo and remove its containers.");
        return outcome;
    }
}
