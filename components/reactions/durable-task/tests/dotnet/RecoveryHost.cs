// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Diagnostics;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;

internal sealed partial class DemoHost
{
    string? recoveryExecutable;

    public async Task InitializeRecoveryAsync(bool skipRustBuild)
    {
        Console.WriteLine($"Artifacts: {ArtifactDirectory}");
        string manifest = Path.GetFullPath(Path.Combine(Root,
            "../../../components/reactions/durable-task/tests/native/Cargo.toml"));
        if (!skipRustBuild)
            await RunAsync("cargo", ["build", "--manifest-path", manifest, "--locked", "--quiet"],
                TimeSpan.FromMinutes(20), cancellation);
        recoveryExecutable = await ResolveExecutableAsync(manifest, "durable-task-recovery-host");
        await InitializeAsync();
        await Database.InitializeAsync(await File.ReadAllTextAsync(
            Path.Combine(AppContext.BaseDirectory, "recovery-schema.sql"), cancellation));
    }

    public async Task StartRecoveryDrasiAsync(string mode = "correlated", string? configPath = null)
    {
        ProcessStartInfo start = StartInfo(
            recoveryExecutable ?? throw new InvalidOperationException("Missing recovery executable"), []);
        start.Environment["DTS_ENDPOINT"] = Endpoint;
        start.Environment["DTS_TASKHUB"] = "default";
        start.Environment["DTS_PRODUCER_ID"] = RunId + "-" + mode;
        start.Environment["APP_STATE_DIR"] = Path.Combine(ArtifactDirectory, mode);
        start.Environment["DTS_REACTION_CONFIG"] = configPath ?? Path.Combine(AppContext.BaseDirectory, "reaction.json");
        string slot = "drasi_" + mode.Replace('-', '_');
        start.Environment["PGHOST"] = "127.0.0.1";
        start.Environment["PGPORT"] = PostgresPort.ToString(CultureInfo.InvariantCulture);
        start.Environment["PGDATABASE"] = "orders";
        start.Environment["PGUSER"] = "drasi";
        start.Environment["PGPASSWORD"] = postgresPassword;
        start.Environment["PGSLOT"] = slot;
        await StartDrasiProcessAsync(start, slot);
    }

    public async Task<JsonNode> ControlAsync(object request)
    {
        Process process = drasi ?? throw new InvalidOperationException("Drasi is not running");
        string json = JsonSerializer.Serialize(request);
        await process.StandardInput.WriteLineAsync(json.AsMemory(), cancellation);
        await process.StandardInput.FlushAsync(cancellation);
        JsonNode response = await ReceiveAsync();
        transcript.Add(new { request = JsonNode.Parse(json), response = response.DeepClone() });
        return response;
    }

    public async Task CrashDrasiAsync()
    {
        if (drasi is { HasExited: false }) drasi.Kill(entireProcessTree: true);
        await StopDrasiAsync();
    }
}
