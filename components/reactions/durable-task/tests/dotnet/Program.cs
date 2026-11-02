// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Runtime.InteropServices;

using CancellationTokenSource shutdown = new();
Console.CancelKeyPress += (_, e) => { e.Cancel = true; shutdown.Cancel(); };
using PosixSignalRegistration? termination = OperatingSystem.IsWindows() ? null :
    PosixSignalRegistration.Create(PosixSignal.SIGTERM, context =>
    {
        context.Cancel = true;
        shutdown.Cancel();
    });
try
{
    if (args.SequenceEqual(["--worker"]))
    {
        await using TestWorker worker = new(
            Environment.GetEnvironmentVariable("DTS_CONNECTION_STRING")
                ?? throw new InvalidOperationException("DTS_CONNECTION_STRING is required"),
            Environment.GetEnvironmentVariable("DTS_EFFECTS_DIR")
                ?? throw new InvalidOperationException("DTS_EFFECTS_DIR is required"));
        await worker.StartAsync(signalReady: true);
        await Console.In.ReadLineAsync(shutdown.Token);
        return 0;
    }
    if (args.Any(arg => arg != "--skip-rust-build"))
        throw new ArgumentException("Usage: dotnet run --project components/reactions/durable-task/tests/dotnet -- [--skip-rust-build]");
    await using DemoHost host = new(shutdown.Token);
    await host.InitializeRecoveryAsync(skipRustBuild: args.Contains("--skip-rust-build"));
    await using WorkerProcess workerProcess = new(host);
    await Verification.RunAsync(host, workerProcess);
    Console.WriteLine($"\nArtifacts: {host.ArtifactDirectory}");
    return 0;
}
catch (OperationCanceledException) when (shutdown.IsCancellationRequested)
{
    Console.Error.WriteLine("Verification cancelled.");
    return 130;
}
catch (Exception error)
{
    Console.Error.WriteLine(error);
    return 1;
}
