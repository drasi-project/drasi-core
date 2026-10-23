// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using Microsoft.DurableTask;
using Microsoft.DurableTask.Worker;
using Microsoft.DurableTask.Worker.AzureManaged;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

internal sealed class TestWorker : IAsyncDisposable
{
    readonly IHost host;
    bool started;

    public TestWorker(string connection, string effectsDirectory)
    {
        HostApplicationBuilder builder = Host.CreateApplicationBuilder();
        builder.Logging.ClearProviders().AddSimpleConsole(options => options.SingleLine = true)
            .SetMinimumLevel(LogLevel.Warning);
        builder.Services.AddDurableTaskWorker(worker =>
        {
            worker.AddTasks(tasks =>
            {
                tasks.AddOrchestrator<OrderWorkflow>();
                tasks.AddOrchestrator<RecoveryWorkflow>();
                tasks.AddOrchestrator<ContinuingWorkflow>();
                tasks.AddOrchestrator<RecordChange>();
                tasks.AddActivity<ReportStatus>();
                tasks.AddActivity(new RecordEffect(effectsDirectory));
            });
            worker.UseDurableTaskScheduler(connection);
        });
        host = builder.Build();
    }

    public async Task StartAsync(bool signalReady = false)
    {
        await host.StartAsync();
        started = true;
        if (signalReady)
            Console.WriteLine("WORKER_READY");
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (started)
                await host.StopAsync();
        }
        finally
        {
            if (host is IAsyncDisposable disposable)
                await disposable.DisposeAsync();
            else
                host.Dispose();
        }
    }
}

public sealed class RecordEffect(string directory) : TaskActivity<Command, string>
{
    public override Task<string> RunAsync(TaskActivityContext context, Command input)
    {
        Directory.CreateDirectory(directory);
        string key = Convert.ToHexString(System.Security.Cryptography.SHA256.HashData(
            System.Text.Encoding.UTF8.GetBytes(input.commandId)));
        string path = Path.Combine(directory, key + ".json");
        string pending = Path.Combine(directory, Guid.NewGuid().ToString("N") + ".tmp");
        try
        {
            using (FileStream file = new(pending, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            {
                System.Text.Json.JsonSerializer.Serialize(file, input);
                file.Flush(flushToDisk: true);
            }
            try
            {
                File.Move(pending, path, overwrite: false);
            }
            catch (IOException) when (File.Exists(path))
            {
                Command? existing = System.Text.Json.JsonSerializer.Deserialize<Command>(
                    File.ReadAllText(path));
                if (existing != input)
                    throw new InvalidOperationException("Idempotency key collision or incomplete effect");
            }
        }
        finally
        {
            File.Delete(pending);
        }
        return Task.FromResult(input.commandId);
    }
}
