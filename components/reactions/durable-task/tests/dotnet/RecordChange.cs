// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using Microsoft.DurableTask;

public sealed class RecordChange : TaskOrchestrator<Command, Command>
{
    public override async Task<Command> RunAsync(TaskOrchestrationContext context, Command input)
    {
        await context.CallActivityAsync(nameof(RecordEffect), input);
        return input;
    }
}
