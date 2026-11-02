// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Runtime.InteropServices;

return await DemoApplication.RunAsync(args);

internal static class DemoApplication
{
    public static async Task<int> RunAsync(string[] args)
    {
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
            if (args.SequenceEqual(["--help"]))
            {
                Console.WriteLine("dotnet run --project examples/lib/durable-task");
                Console.WriteLine("Enter advances the walkthrough; q or Ctrl+C exits.");
                return 0;
            }
            if (args.Length > 0)
                throw new ArgumentException("Unknown argument. Use --help for usage.");

            Console.WriteLine("Preparing PostgreSQL, Drasi and Durable Task Scheduler...");
            await using DemoHost host = new(shutdown.Token);
            await host.InitializeAsync();
            await host.StartDrasiAsync();
            await using WorkflowWorker demoWorker = new(host.ConnectionString);
            await demoWorker.StartAsync();
            DemoConsole console = new(Console.In, Console.Out, shutdown.Token);
            string? outcome = await Walkthrough.RunAsync(host, console);
            if (outcome is null)
            {
                Console.WriteLine($"\nWalkthrough cancelled. No further SQL will run.\nArtifacts: {host.ArtifactDirectory}");
                return 130;
            }
            await host.SaveResultAsync(new
            {
                scenario = "postgres-order-workflow",
                outcome,
                postgresImage = DemoHost.PostgresImage,
                postgresPort = host.PostgresPort,
            });
            Console.WriteLine($"\nArtifacts: {host.ArtifactDirectory}");
            Console.WriteLine("Cleaning up the local demo containers.");
            return 0;
        }
        catch (OperationCanceledException) when (shutdown.IsCancellationRequested)
        {
            Console.Error.WriteLine("Example cancelled.");
            return 130;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine(error);
            return 1;
        }
    }
}
