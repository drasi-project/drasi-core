// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Diagnostics;
using System.Reflection;
using static DemoHost;

internal sealed class WorkerProcess(DemoHost host) : IAsyncDisposable
{
    Process? process;
    Task logs = Task.CompletedTask;

    public async Task StartAsync()
    {
        Require(process is null, "Stop the previous worker first");
        ProcessStartInfo start = StartInfo("dotnet", [Assembly.GetExecutingAssembly().Location, "--worker"]);
        start.Environment["DTS_CONNECTION_STRING"] = host.ConnectionString;
        start.Environment["DTS_EFFECTS_DIR"] = Path.Combine(host.ArtifactDirectory, "effects");
        process = Process.Start(start) ?? throw new InvalidOperationException("Could not start Durable Task worker");
        TaskCompletionSource ready = new(TaskCreationOptions.RunContinuationsAsynchronously);
        logs = Task.WhenAll(
            host.DrainAsync(process.StandardOutput, "worker.log", line =>
            {
                if (line.StartsWith("WORKER_READY", StringComparison.Ordinal))
                    ready.TrySetResult();
            }),
            host.DrainAsync(process.StandardError, "worker-error.log"));
        Task first = await Task.WhenAny(ready.Task, logs).WaitAsync(TimeSpan.FromSeconds(60), host.Cancellation);
        Require(first == ready.Task, "Durable Task worker exited before readiness; inspect worker.log");
        await ready.Task;
    }

    public async Task StopAsync(bool crash = false)
    {
        if (process is null) return;
        if (crash && !process.HasExited) process.Kill(entireProcessTree: true);
        await DemoHost.StopAsync(process);
        await logs;
        process.Dispose();
        process = null;
    }

    public async ValueTask DisposeAsync() => await StopAsync();
}
