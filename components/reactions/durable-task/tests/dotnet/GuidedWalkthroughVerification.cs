// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using DurableTask.Core;
using static DemoHost;

internal static class GuidedWalkthroughVerification
{
    public static async Task RunAsync(DemoHost host)
    {
        int writesBefore = host.Database.Writes.Count;
        foreach (string reply in new[] { "q\n", "", "invalid\nq\n" })
        {
            using StringReader input = new(reply);
            DemoConsole cancelled = new(input, TextWriter.Null, host.Cancellation);
            Require(await Walkthrough.RunAsync(host, cancelled) is null, "Quit/EOF did not cancel the walkthrough");
            Require(host.Database.Writes.Count == writesBefore, "Cancellation submitted SQL");
        }

        using PromptInput prompts = new(host);
        using StringWriter output = new();
        DemoConsole console = new(prompts, output, host.Cancellation);
        string outcome = await Walkthrough.RunAsync(host, console)
            ?? throw new InvalidOperationException("Walkthrough unexpectedly cancelled");
        OrderRow? row = await host.Database.ReadOrderAsync(Walkthrough.OrderId);
        Require(prompts.Reads == 4 && row is { Status: "delivered" },
            "The guided steps did not leave the expected database row");
        Require(host.Database.Writes.Count == writesBefore + 4, "Walkthrough submitted unexpected SQL writes");
        OrchestrationState completed = await host.StateWhenAsync(Walkthrough.OrderId,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Require(Read<OrderStatus>(completed.Input) == new OrderStatus(Walkthrough.OrderId, "placed")
            && Read<string>(completed.Status) == "delivered"
            && prompts.States.Count == 3
            && prompts.States.Select(state => state.OrchestrationInstance.ExecutionId)
                .Append(completed.OrchestrationInstance.ExecutionId).Distinct().Count() == 1,
            "The status updates did not reach the same workflow instance");
        string text = output.ToString();
        Require(new[] { "Status: placed", "Status: paid", "Status: shipped", "Status: delivered", "Workflow: COMPLETED" }
            .All(text.Contains), "Observed states were missing from the walkthrough output");
        Require(text.Split("Query change: ADDED").Length == 2
            && text.Split("Query change: UPDATED").Length == 4 && !text.Contains("REMOVED"),
            "Expected one ADD and three UPDATEs, including delivery");
        Require(text.Split("SQL:").Length == 5, "Walkthrough did not display the four executed SQL statements");
        Require(outcome == $"Order {Walkthrough.OrderId} delivered." && !text.Contains("WORKER_READY"),
            "Unexpected workflow output or an internal readiness message leaked into the walkthrough");
        Require(!text.Contains("exception", StringComparison.OrdinalIgnoreCase),
            "The walkthrough used error terminology for an order status");
        await File.WriteAllTextAsync(Path.Combine(host.ArtifactDirectory, "walkthrough.log"), text, host.Cancellation);
        host.Database.CommandOutput = null;

        string otherOrder = "order-" + host.RunId;
        await host.Database.PlaceOrderAsync(otherOrder);
        OrchestrationState otherStarted = await host.WaitForStatusAsync(otherOrder, "placed");
        await host.Database.UpdateStatusAsync(otherOrder, "delivered");
        await host.Database.UpdateStatusAsync(otherOrder, "packing");
        await host.Database.UpdateStatusAsync(otherOrder, "paid");
        OrchestrationState otherPaid = await host.WaitForStatusAsync(otherOrder, "paid");
        Require(otherPaid.OrchestrationStatus == OrchestrationStatus.Running
            && otherPaid.OrchestrationInstance.ExecutionId == otherStarted.OrchestrationInstance.ExecutionId,
            "An early delivery or unrelated event skipped the payment/shipment waits");
        await host.Database.UpdateStatusAsync(otherOrder, "shipped");
        OrchestrationState otherCompleted = await host.StateWhenAsync(otherOrder,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Require(otherCompleted.OrchestrationInstance.ExecutionId == otherStarted.OrchestrationInstance.ExecutionId
            && Read<string>(otherCompleted.Output) == $"Order {otherOrder} delivered."
            && Read<string>(otherCompleted.Status) == "delivered"
            && await host.Database.ReadOrderAsync(otherOrder) is { Status: "shipped" },
            "The workflow did not consume the buffered delivery event after payment and shipment");
    }

    sealed class PromptInput(DemoHost host) : TextReader
    {
        public int Reads { get; private set; }
        public List<OrchestrationState> States { get; } = [];

        public override async ValueTask<string?> ReadLineAsync(CancellationToken cancellationToken)
        {
            OrderRow? row = await host.Database.ReadOrderAsync(Walkthrough.OrderId);
            bool expected = Reads switch
            {
                0 => row is null,
                1 => row is { Status: "placed" },
                2 => row is { Status: "paid" },
                3 => row is { Status: "shipped" },
                _ => false,
            };
            Require(expected, $"SQL ran before prompt {Reads + 1}, or an unexpected prompt was shown");
            if (row is not null)
            {
                OrchestrationState state = await host.WaitForStatusAsync(Walkthrough.OrderId, row.Status);
                Require(state.OrchestrationStatus == OrchestrationStatus.Running,
                    "The workflow completed before delivery");
                States.Add(state);
            }
            Reads++;
            return "";
        }
    }
}
