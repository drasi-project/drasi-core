// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Text.Json;
using DurableTask.Core;

internal static class RecoveryAssertions
{
    public static Task<OrchestrationState> AppliedAsync(
        this DemoHost host, string instanceId, int count, long revision, int timeoutSeconds = 60) =>
        host.StateWhenAsync(instanceId, state =>
        {
            Progress? progress = ProgressOf(state);
            return progress?.applied.Length == count && progress.lastCommand.revision == revision;
        }, timeoutSeconds);

    public static Progress? ProgressOf(OrchestrationState state) =>
        string.IsNullOrEmpty(state.Status) ? null : JsonSerializer.Deserialize<Progress>(state.Status);

    public static Command[] ReadEffects(this DemoHost host)
    {
        string directory = Path.Combine(host.ArtifactDirectory, "effects");
        return Directory.Exists(directory)
            ? Directory.GetFiles(directory, "*.json").Select(path => DemoHost.Read<Command>(File.ReadAllText(path))).ToArray()
            : [];
    }
}
