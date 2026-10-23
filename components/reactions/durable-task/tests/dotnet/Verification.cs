// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Diagnostics;
using System.Text.Json;
using System.Text.Json.Nodes;
using DurableTask.Core;
using DurableTask.Core.Exceptions;
using Grpc.Core;
using static DemoHost;
using static RecoveryAssertions;

internal static class Verification
{
    public static async Task RunAsync(DemoHost host, WorkerProcess worker)
    {
        List<object> checks = [];
        void Check(string name, bool condition, string detail)
        {
            Require(condition, $"{name}: {detail}");
            checks.Add(new { name, detail });
            Console.WriteLine($"PASS {name}: {detail}");
        }

        string episode = "drasi-" + host.RunId;
        string configPath = Path.Combine(AppContext.BaseDirectory, "reaction.json");
        await host.StartRecoveryDrasiAsync(configPath: configPath);
        string bufferedEpisode = episode + "-buffered";
        JsonNode bufferedCheckpoint = await host.ControlAsync(new { action = "checkpoint" });
        await host.Database.InsertOrderAsync(bufferedEpisode, 1, "queued before worker");
        bufferedCheckpoint = await host.ControlAsync(new
        { action = "awaitChange", after = bufferedCheckpoint["sequence"]!.GetValue<long>() });
        await host.Database.UpdateOrderAsync(bufferedEpisode, 2, "updated before worker");
        bufferedCheckpoint = await host.ControlAsync(new
        { action = "awaitChange", after = bufferedCheckpoint["sequence"]!.GetValue<long>() });
        await host.Database.UpdateOrderAsync(bufferedEpisode, 3, "resolved before worker", needsAttention: false);
        await host.ControlAsync(new
        { action = "awaitChange", after = bufferedCheckpoint["sequence"]!.GetValue<long>() });
        OrchestrationState pending = await host.StateWhenAsync(bufferedEpisode, _ => true);
        Require(pending.OrchestrationStatus == OrchestrationStatus.Pending && host.ReadEffects().Length == 0,
            "The buffered workflow must remain pending with no effects until the worker starts");

        await worker.StartAsync();
        OrchestrationState buffered = await host.StateWhenAsync(bufferedEpisode,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Outcome bufferedOutcome = Read<Outcome>(buffered.Output);
        Dictionary<string, Command> bufferedEffects = host.ReadEffects()
            .Where(value => value.episodeId == bufferedEpisode).ToDictionary(value => value.commandId);
        Check("buffered-external-events", bufferedEffects.Count == 3 && bufferedOutcome.applied.Length == 3
            && bufferedOutcome.applied.Select(id => bufferedEffects[id])
                .Select(value => (value.kind, value.revision, value.reason)).SequenceEqual(
                    [("ADD", 1L, "queued before worker"), ("UPDATE", 2L, "updated before worker"),
                     ("DELETE", 2L, "updated before worker")]),
            "The SDK consumed UPDATE and DELETE queued before the first event wait, in order and without a custom inbox");

        JsonNode baseline = await host.ControlAsync(new { action = "checkpoint" });
        await host.Database.InsertOrderAsync(episode, 1, "inventory missing");
        JsonNode inserted = await host.ControlAsync(new { action = "awaitChange", after = baseline["sequence"]!.GetValue<long>() });
        OrchestrationState first = await host.AppliedAsync(episode, 1, 1);
        Command initial = Read<Command>(first.Input);
        string execution = first.OrchestrationInstance.ExecutionId;
        Check("configured-start", JsonNode.DeepEquals(inserted["checkpoint"], inserted["sequence"])
            && first.Tags.TryGetValue("drasi-command-id", out string? tag) && tag == initial.commandId,
            "The native reaction started a real .NET Durable Task workflow and durably checkpointed");

        await RejectDuplicateStartAsync(host, initial);
        OrchestrationState duplicate = await host.StateWhenAsync(episode, _ => true);
        Check("duplicate-running-start", duplicate.OrchestrationInstance.ExecutionId == execution,
            "The official .NET client rejected a duplicate without replacing the execution");

        await host.Database.UpdateOrderAsync(episode, 2, "supplier delayed");
        Command second = ProgressOf(await host.AppliedAsync(episode, 2, 2))!.lastCommand!;
        await host.Client.RaiseEventAsync(new OrchestrationInstance { InstanceId = episode }, "condition", second)
            .WaitAsync(TimeSpan.FromSeconds(20), host.Cancellation);
        await host.StateWhenAsync(episode, state => ProgressOf(state)?.duplicateEvents == 1);
        Check("duplicate-event", true, "The workflow deduplicated a repeated command");

        await worker.StopAsync(crash: true);
        baseline = await host.ControlAsync(new { action = "checkpoint" });
        await host.Database.UpdateOrderAsync(episode, 3, "replacement found");
        await host.ControlAsync(new { action = "awaitChange", after = baseline["sequence"]!.GetValue<long>() });
        OrchestrationState offline = await host.StateWhenAsync(episode, _ => true);
        Check("offline-worker", ProgressOf(offline)?.applied.Length == 2,
            "DTS accepted an event with no .NET worker running");
        Stopwatch recovery = Stopwatch.StartNew();
        await worker.StartAsync();
        OrchestrationState recovered = await host.AppliedAsync(episode, 3, 3, timeoutSeconds: 240);
        double recoverySeconds = Math.Round(recovery.Elapsed.TotalSeconds, 2);
        Command third = ProgressOf(recovered)!.lastCommand!;
        Check("worker-recovery", recovered.OrchestrationInstance.ExecutionId == execution,
            $"Worker replay resumed the same execution in {recoverySeconds}s");

        await host.ControlAsync(new { action = "pauseReaction" });
        baseline = await host.ControlAsync(new { action = "checkpoint" });
        await host.Database.UpdateOrderAsync(episode, 4, "warehouse assigned");
        JsonNode queued = await host.ControlAsync(new
        { action = "awaitChange", after = baseline["sequence"]!.GetValue<long>(), outbox = true });
        Check("durable-outbox", queued["sequence"]!.GetValue<long>() > queued["checkpoint"]!.GetValue<long>(),
            "A query change was persisted while the reaction was stopped");
        await host.CrashDrasiAsync();
        await host.StartRecoveryDrasiAsync(configPath: configPath);
        Command fourth = ProgressOf(await host.AppliedAsync(episode, 4, 4))!.lastCommand!;
        JsonNode restored = await host.ControlAsync(new { action = "checkpoint" });
        Check("drasi-process-recovery", JsonNode.DeepEquals(restored["sequence"], queued["sequence"]),
            "SIGKILL/restart reloaded RocksDB/checkpoints and delivered the pending update");

        await host.Database.UpdateOrderAsync(episode, 5, "resolved", needsAttention: false);
        OrchestrationState completed = await host.StateWhenAsync(episode,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Outcome outcome = Read<Outcome>(completed.Output);
        Command[] relevant = host.ReadEffects().Where(value => value.episodeId == episode).ToArray();
        Command deleted = relevant.Single(value => value.kind == "DELETE");
        Command[] expected =
        [
            new(initial.commandId, episode, "ADD", 1, "inventory missing"),
            new(second.commandId, episode, "UPDATE", 2, "supplier delayed"),
            new(third.commandId, episode, "UPDATE", 3, "replacement found"),
            new(fourth.commandId, episode, "UPDATE", 4, "warehouse assigned"),
            new(deleted.commandId, episode, "DELETE", 4, "warehouse assigned"),
        ];
        Check("delete-before-image", deleted.revision == 4 && outcome.finalRevision == 4,
            "DELETE carried the last matching row, not the clearing source revision 5");
        Check("exact-effects", relevant.Length == 5 && outcome.applied.Length == 5
            && outcome.duplicateEvents == 1
            && outcome.applied.ToHashSet().SetEquals(expected.Select(value => value.commandId))
            && relevant.OrderBy(value => value.commandId, StringComparer.Ordinal)
                .SequenceEqual(expected.OrderBy(value => value.commandId, StringComparer.Ordinal)),
            "Exactly five full command payloads were applied across both process restarts");

        await RejectDuplicateStartAsync(host, initial);
        OrchestrationState repeated = await host.StateWhenAsync(episode, _ => true);
        Check("duplicate-completed-start", repeated.Output == completed.Output
            && repeated.OrchestrationInstance.ExecutionId == execution,
            "Completed instance was not replaced");
        Command late = deleted with { commandId = episode + "-late", kind = "UPDATE", revision = 9 };
        await host.Client.RaiseEventAsync(new OrchestrationInstance { InstanceId = episode }, "condition", late)
            .WaitAsync(TimeSpan.FromSeconds(20), host.Cancellation);
        string missingCode;
        try
        {
            Command missing = late with { episodeId = episode + "-missing" };
            await host.Client.RaiseEventAsync(new OrchestrationInstance { InstanceId = missing.episodeId },
                "condition", missing).WaitAsync(TimeSpan.FromSeconds(20), host.Cancellation);
            missingCode = "OK";
        }
        catch (RpcException error) when (error.StatusCode == StatusCode.NotFound)
        {
            missingCode = error.StatusCode.ToString();
        }
        Check("event-contract", missingCode == "NotFound",
            "The official .NET client observed completed-event OK versus missing-instance NotFound");

        string replayEpisode = episode + "-postgres";
        await host.Database.InsertOrderAsync(replayEpisode, 1, "in database");
        OrchestrationState beforeCrash = await host.AppliedAsync(replayEpisode, 1, 1);
        await host.CrashDrasiAsync();
        await host.Database.UpdateOrderAsync(replayEpisode, 2, "committed while Drasi was stopped");
        await host.StartRecoveryDrasiAsync(configPath: configPath);
        OrchestrationState replayed = await host.AppliedAsync(replayEpisode, 2, 2);
        Check("postgres-wal-recovery",
            replayed.OrchestrationInstance.ExecutionId == beforeCrash.OrchestrationInstance.ExecutionId
            && ProgressOf(replayed)?.lastCommand?.reason == "committed while Drasi was stopped",
            "A SQL update committed with Drasi offline was replayed from PostgreSQL WAL");
        await host.Database.DeleteOrderAsync(replayEpisode);
        OrchestrationState removed = await host.StateWhenAsync(replayEpisode,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Outcome removedOutcome = Read<Outcome>(removed.Output);
        Command[] databaseEffects = host.ReadEffects().Where(value => value.episodeId == replayEpisode).ToArray();
        Check("postgres-row-delete", databaseEffects.Length == 3 && removedOutcome.finalRevision == 2
            && removedOutcome.applied.Length == 3
            && removedOutcome.applied.ToHashSet().SetEquals(databaseEffects.Select(value => value.commandId))
            && databaseEffects.Select(value => (value.kind, value.revision, value.reason)).ToHashSet().SetEquals(
                [("ADD", 1L, "in database"), ("UPDATE", 2L, "committed while Drasi was stopped"),
                 ("DELETE", 2L, "committed while Drasi was stopped")]),
            "A physical SQL DELETE completed the workflow with the exact previous-row payload");
        await host.StopDrasiAsync();

        JsonNode config = JsonNode.Parse(await File.ReadAllTextAsync(configPath))!;
        JsonNode snapshotConfig = config.DeepClone();
        snapshotConfig["defaultTemplate"] = new JsonObject
        {
            ["added"] = new JsonObject { ["action"] = "ignore" },
            ["updated"] = new JsonObject
            {
                ["action"] = "start",
                ["orchestration"] = "RecordChange",
                ["template"] = config["defaultTemplate"]!["deleted"]!["template"]!.GetValue<string>(),
            },
            ["deleted"] = new JsonObject { ["action"] = "ignore" },
        };
        string snapshotPath = Path.Combine(host.ArtifactDirectory, "bootstrap.json");
        await File.WriteAllTextAsync(snapshotPath, snapshotConfig.ToJsonString(JsonOptions));
        string snapshotEpisode = episode + "-snapshot";
        await host.Database.InsertOrderAsync(snapshotEpisode, 1, "present before query startup");
        await host.StartRecoveryDrasiAsync("bootstrap", snapshotPath);
        JsonNode snapshotCheckpoint = await host.ControlAsync(new { action = "checkpoint" });
        await host.Database.UpdateOrderAsync(snapshotEpisode, 2, "updated after bootstrap");
        Command snapshotEffect = await EffectAsync(host, snapshotEpisode, "UPDATE");
        OrchestrationState snapshotCompleted = await host.StateWhenAsync("drasi-" + snapshotEffect.commandId,
            state => state.OrchestrationStatus == OrchestrationStatus.Completed);
        Check("postgres-bootstrap", snapshotCheckpoint["sequence"]!.GetValue<long>() == 0
            && snapshotEffect.revision == 1 && snapshotEffect.reason == "present before query startup"
            && Read<Command>(snapshotCompleted.Output) == snapshotEffect
            && host.ReadEffects().Count(value => value.episodeId == snapshotEpisode) == 1,
            "Snapshot populated query state without emitting ADD; live UPDATE carried the exact snapshot before-image");
        await host.Database.DeleteOrderAsync(snapshotEpisode);
        await host.StopDrasiAsync();

        JsonNode perChange = config.DeepClone();
        foreach (KeyValuePair<string, JsonNode?> mapping in perChange["defaultTemplate"]!.AsObject())
        {
            JsonObject spec = mapping.Value!.AsObject();
            string template = spec["template"]!.GetValue<string>();
            spec.Clear();
            spec["action"] = "start";
            spec["orchestration"] = "RecordChange";
            spec["template"] = template;
        }
        string perChangePath = Path.Combine(host.ArtifactDirectory, "per-change.json");
        await File.WriteAllTextAsync(perChangePath, perChange.ToJsonString(JsonOptions));
        await host.StartRecoveryDrasiAsync("per-change", perChangePath);
        string changeEpisode = episode + "-changes";
        List<Command> changes = [];
        foreach ((string action, int revision, string kind) in new[]
            { ("insert", 1, "ADD"), ("update", 2, "UPDATE"), ("clear", 3, "DELETE") })
        {
            if (action == "insert")
                await host.Database.InsertOrderAsync(changeEpisode, revision, action);
            else
                await host.Database.UpdateOrderAsync(changeEpisode, revision, action, needsAttention: action != "clear");
            Command effect = await EffectAsync(host, changeEpisode, kind);
            OrchestrationState result = await host.StateWhenAsync("drasi-" + effect.commandId,
                state => state.OrchestrationStatus == OrchestrationStatus.Completed);
            Require(Read<Command>(result.Output) == effect, "Per-change workflow output mismatch");
            changes.Add(effect);
        }
        Check("start-on-every-operation", changes.Select(value => (value.kind, value.revision, value.reason))
            .ToHashSet().SetEquals([("ADD", 1L, "insert"), ("UPDATE", 2L, "update"), ("DELETE", 2L, "update")]),
            "Config alone selected three separate workflows with internally generated IDs");
        await host.StopDrasiAsync();

        JsonNode terminate = config.DeepClone();
        terminate["defaultTemplate"]!["updated"] = new JsonObject { ["action"] = "ignore" };
        terminate["defaultTemplate"]!["deleted"] = new JsonObject
        {
            ["action"] = "terminate",
            ["instanceId"] = "{{before.episodeId}}",
            ["onMissing"] = "error",
            ["recursive"] = false,
            ["template"] = "\"removed from query\"",
        };
        string terminatePath = Path.Combine(host.ArtifactDirectory, "terminate.json");
        await File.WriteAllTextAsync(terminatePath, terminate.ToJsonString(JsonOptions));
        await host.StartRecoveryDrasiAsync("terminate", terminatePath);
        string terminationEpisode = episode + "-terminate";
        await host.Database.InsertOrderAsync(terminationEpisode, 1, "active");
        await host.AppliedAsync(terminationEpisode, 1, 1);
        await host.Database.UpdateOrderAsync(terminationEpisode, 2, "ignored");
        await host.Database.UpdateOrderAsync(terminationEpisode, 3, "terminate", needsAttention: false);
        await host.StateWhenAsync(terminationEpisode, state => state.OrchestrationStatus == OrchestrationStatus.Terminated);
        Check("configured-ignore-and-terminate",
            host.ReadEffects().Count(value => value.episodeId == terminationEpisode) == 1,
            "Config alone ignored UPDATE and force-terminated on DELETE without extra effects");
        await host.StopDrasiAsync();
        await host.StartDrasiAsync();
        await GuidedWalkthroughVerification.RunAsync(host);
        Check("guided-walkthrough", true,
            "Named event waits preserved stage order; prompts preceded SQL; quit/EOF changed no rows; the local dashboard stayed available through the final inspection pause");
        Check("no-late-effects", host.ReadEffects().Length == 16,
            "Thirteen original integration effects plus three buffered-event effects, with no extras");
        await host.SaveResultAsync(new
        {
            image = EmulatorImage,
            postgresImage = PostgresImage,
            postgresPort = host.PostgresPort,
            endpoint = host.Endpoint,
            checks,
            outcome,
            workerRecoverySeconds = recoverySeconds,
            effects = host.ReadEffects(),
            versions = JsonNode.Parse(await File.ReadAllTextAsync(Path.Combine(AppContext.BaseDirectory, "packages.lock.json"))),
            limitations = new[]
            {
                "DTS and PostgreSQL stayed running during process-crash checks; their restart persistence was not tested.",
                "Managed Azure auth, RBAC and TLS connectivity are not exercised.",
                "Event RPC acknowledgement is enqueueing, not workflow consumption.",
                "The file activity sink is a demonstration, not a production transaction system.",
            },
        });
        Console.WriteLine($"PASS all {checks.Count} checks");
    }

    static async Task<Command> EffectAsync(DemoHost host, string episode, string kind)
    {
        Stopwatch waiting = Stopwatch.StartNew();
        while (waiting.Elapsed < TimeSpan.FromSeconds(60))
        {
            Command? effect = host.ReadEffects().SingleOrDefault(value =>
                value.episodeId == episode && value.kind == kind);
            if (effect is not null) return effect;
            await Task.Delay(100, host.Cancellation);
        }
        throw new TimeoutException($"Missing {kind} activity effect for {episode}");
    }

    static async Task RejectDuplicateStartAsync(DemoHost host, Command command)
    {
        try
        {
            await host.Client.CreateOrchestrationInstanceAsync(nameof(RecoveryWorkflow), "",
                command.episodeId, command, tags: null, Enum.GetValues<OrchestrationStatus>())
                .WaitAsync(TimeSpan.FromSeconds(20), host.Cancellation);
        }
        catch (OrchestrationAlreadyExistsException)
        {
            return;
        }
        catch (RpcException error) when (error.StatusCode == StatusCode.AlreadyExists)
        {
            return;
        }
        throw new InvalidOperationException("Duplicate orchestration start was unexpectedly accepted");
    }
}
