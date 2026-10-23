// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using Microsoft.DurableTask;

public sealed class RecoveryWorkflow : TaskOrchestrator<Command, Outcome>
{
    public override async Task<Outcome> RunAsync(TaskOrchestrationContext context, Command order)
    {
        await context.CallActivityAsync(nameof(RecordEffect), order);
        HashSet<string> applied = [order.commandId];
        int duplicates = 0;

        while (order.kind != "DELETE")
        {
            context.SetCustomStatus(new Progress(applied.ToArray(), duplicates, order));
            Command change = await context.WaitForExternalEvent<Command>("condition");
            if (change.episodeId != order.episodeId || change.kind is not ("UPDATE" or "DELETE"))
                throw new InvalidOperationException("Expected an UPDATE or DELETE for this order");
            if (!applied.Add(change.commandId))
            {
                duplicates++;
                continue;
            }
            if (change.revision < order.revision)
                throw new InvalidOperationException("Out-of-order business revision");

            await context.CallActivityAsync(nameof(RecordEffect), change);
            order = change;
        }

        context.SetCustomStatus(new Progress(applied.ToArray(), duplicates, order));
        return new Outcome(order.episodeId, order.revision, applied.ToArray(), duplicates);
    }
}

public record Command(string commandId, string episodeId, string kind, long revision, string reason);
public record Progress(string[] applied, int duplicateEvents, Command lastCommand);
public record Outcome(string episodeId, long finalRevision, string[] applied, int duplicateEvents);
