// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using Microsoft.DurableTask.Worker;
using Microsoft.DurableTask.Worker.AzureManaged;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Microsoft.Extensions.Logging;

internal sealed class WorkflowWorker : IAsyncDisposable
{
    readonly IHost host;
    bool started;

    public WorkflowWorker(string connection)
    {
        HostApplicationBuilder builder = Host.CreateApplicationBuilder();
        builder.Logging.ClearProviders().AddSimpleConsole(options => options.SingleLine = true)
            .SetMinimumLevel(LogLevel.Warning);
        builder.Services.AddDurableTaskWorker(worker =>
        {
            worker.AddTasks(tasks =>
            {
                tasks.AddOrchestrator<OrderWorkflow>();
                tasks.AddActivity<ReportStatus>();
            });
            worker.UseDurableTaskScheduler(connection);
        });
        host = builder.Build();
    }

    public async Task StartAsync()
    {
        await host.StartAsync();
        started = true;
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            if (started) await host.StopAsync();
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
