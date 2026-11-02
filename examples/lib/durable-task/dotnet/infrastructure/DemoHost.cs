// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

using System.Diagnostics;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Nodes;
using DurableTask.Core;
using Grpc.Core;
using Microsoft.DurableTask.AzureManagedBackend;
using Microsoft.Extensions.Logging;
using Npgsql;

internal sealed partial class DemoHost : IAsyncDisposable
{
    public static readonly JsonSerializerOptions JsonOptions = new() { WriteIndented = true };
    public const string EmulatorImage = "mcr.microsoft.com/dts/dts-emulator@sha256:361323065a608d605f9d3ae56b854eb11f1c47fcc16845a37f948f03ca9c5fac";
    public const string PostgresImage = "postgres@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777";
    readonly CancellationToken cancellation;
    readonly List<object> transcript = [];
    readonly string postgresPassword = Guid.NewGuid().ToString("N");
    readonly ILoggerFactory logging = LoggerFactory.Create(builder => builder.SetMinimumLevel(LogLevel.Warning));
    AzureManagedOrchestrationService? service;
    TaskHubClient? client;
    Process? drasi;
    Task drasiLog = Task.CompletedTask;
    string? container;
    string? postgresContainer;
    OrdersDatabase? database;
    string? rustExecutable;

    public string Root { get; } = FindRoot();
    public string RunId { get; } = Guid.NewGuid().ToString("N")[..12];
    public string ArtifactDirectory { get; }
    public string Endpoint { get; private set; } = "";
    public string DashboardUrl { get; private set; } = "";
    public int PostgresPort { get; private set; }
    public string ConnectionString => $"Endpoint={Endpoint};TaskHub=default;Authentication=None";
    public TaskHubClient Client => client ?? throw new InvalidOperationException("Host has not been initialized");
    public CancellationToken Cancellation => cancellation;
    public OrdersDatabase Database => database ?? throw new InvalidOperationException("PostgreSQL has not been initialized");

    public DemoHost(CancellationToken cancellation)
    {
        this.cancellation = cancellation;
        ArtifactDirectory = Path.Combine(Root, "artifacts",
            $"{DateTime.UtcNow:yyyyMMddTHHmmssZ}-{RunId}");
        Directory.CreateDirectory(ArtifactDirectory);
    }

    public async Task InitializeAsync()
    {
        container = $"drasi-dts-example-{RunId}";
        await RunAsync("docker", ["run", "-d", "--name", container,
            "-p", "127.0.0.1::8080", "-p", "127.0.0.1::8082", EmulatorImage],
            TimeSpan.FromMinutes(3), cancellation);
        string binding = (await RunAsync("docker", ["port", container, "8080/tcp"],
            TimeSpan.FromSeconds(20), cancellation)).Trim();
        Endpoint = "http://" + binding;
        string dashboardBinding = (await RunAsync("docker", ["port", container, "8082/tcp"],
            TimeSpan.FromSeconds(20), cancellation)).Trim();
        DashboardUrl = "http://" + dashboardBinding;
        service = new(AzureManagedOrchestrationServiceOptions.FromConnectionString(ConnectionString), logging);
        client = new(service);
        Stopwatch elapsed = Stopwatch.StartNew();
        while (true)
        {
            try
            {
                await Client.GetOrchestrationStateAsync("readiness-" + RunId)
                    .WaitAsync(TimeSpan.FromSeconds(10), cancellation);
                break;
            }
            catch (RpcException error) when (elapsed.Elapsed < TimeSpan.FromSeconds(30)
                && (error.StatusCode is StatusCode.Unavailable or StatusCode.Unknown or StatusCode.DeadlineExceeded
                    || error.StatusCode == StatusCode.Internal && error.InnerException is HttpRequestException))
            {
                await Task.Delay(250, cancellation);
            }
        }
        postgresContainer = $"drasi-dts-postgres-{RunId}";
        await RunAsync("docker", ["run", "-d", "--name", postgresContainer,
            "-p", "127.0.0.1::5432",
            "--env", "POSTGRES_USER=drasi", "--env", "POSTGRES_DB=orders",
            "--env", "POSTGRES_PASSWORD", PostgresImage,
            "postgres", "-c", "wal_level=logical", "-c", "max_replication_slots=10", "-c", "max_wal_senders=10"],
            TimeSpan.FromMinutes(3), cancellation,
            environment: new Dictionary<string, string> { ["POSTGRES_PASSWORD"] = postgresPassword });
        string postgresBinding = (await RunAsync("docker", ["port", postgresContainer, "5432/tcp"],
            TimeSpan.FromSeconds(20), cancellation)).Trim();
        PostgresPort = int.Parse(postgresBinding[(postgresBinding.LastIndexOf(':') + 1)..], CultureInfo.InvariantCulture);
        NpgsqlConnectionStringBuilder connection = new()
        {
            Host = "127.0.0.1",
            Port = PostgresPort,
            Database = "orders",
            Username = "drasi",
            Password = postgresPassword,
            SslMode = SslMode.Disable,
            Timeout = 3,
            CommandTimeout = 15,
        };
        database = new(connection.ConnectionString, cancellation);
        await database.InitializeAsync(await File.ReadAllTextAsync(Path.Combine(Root, "schema.sql"), cancellation));
    }

    public async Task StartDrasiAsync()
    {
        if (rustExecutable is null)
        {
            Console.WriteLine("Building the native Drasi host...");
            string manifest = Path.Combine(Root, "Cargo.toml");
            await RunAsync("cargo", ["build", "--manifest-path", manifest, "--locked", "--quiet"],
                TimeSpan.FromMinutes(20), cancellation);
            rustExecutable = await ResolveExecutableAsync(manifest, "durable-task-example");
        }
        ProcessStartInfo start = StartInfo(rustExecutable, []);
        start.Environment["DTS_ENDPOINT"] = Endpoint;
        start.Environment["DTS_TASKHUB"] = "default";
        start.Environment["DTS_PRODUCER_ID"] = RunId;
        start.Environment["APP_STATE_DIR"] = Path.Combine(ArtifactDirectory, "state");
        start.Environment["PGHOST"] = "127.0.0.1";
        start.Environment["PGPORT"] = PostgresPort.ToString(CultureInfo.InvariantCulture);
        start.Environment["PGDATABASE"] = "orders";
        start.Environment["PGUSER"] = "drasi";
        start.Environment["PGPASSWORD"] = postgresPassword;
        await StartDrasiProcessAsync(start, "drasi_orders");
    }

    async Task<string> ResolveExecutableAsync(string manifest, string name)
    {
        string metadata = await RunAsync("cargo", ["metadata", "--manifest-path",
            manifest, "--no-deps", "--format-version", "1"], TimeSpan.FromMinutes(1), cancellation);
        string target = JsonNode.Parse(metadata)!["target_directory"]!.GetValue<string>();
        string executable = Path.Combine(target, "debug", OperatingSystem.IsWindows() ? name + ".exe" : name);
        Require(File.Exists(executable), $"Build the Drasi host first: {executable}");
        return executable;
    }

    async Task StartDrasiProcessAsync(ProcessStartInfo start, string slot)
    {
        Require(drasi is null, "Stop the previous Drasi host first");
        drasi = Process.Start(start) ?? throw new InvalidOperationException("Could not start Drasi");
        drasiLog = DrainAsync(drasi.StandardError, "drasi.log");
        JsonNode ready = await ReceiveAsync();
        Require(ready["ready"]?.GetValue<bool>() == true, "Drasi did not become ready");
        await Database.WaitForReplicationAsync(slot);
    }

    async Task<JsonNode> ReceiveAsync()
    {
        Process process = drasi ?? throw new InvalidOperationException("Drasi is not running");
        string? line = await process.StandardOutput.ReadLineAsync(cancellation)
            .AsTask().WaitAsync(TimeSpan.FromSeconds(75), cancellation);
        return line is null
            ? throw new InvalidOperationException("Drasi exited; inspect drasi.log")
            : JsonNode.Parse(line) ?? throw new InvalidOperationException("Drasi returned null JSON");
    }

    public async Task StopDrasiAsync()
    {
        if (drasi is null) return;
        await StopAsync(drasi);
        await drasiLog;
        drasi.Dispose();
        drasi = null;
    }

    public async Task<OrchestrationState> StateWhenAsync(
        string instanceId, Func<OrchestrationState, bool> predicate, int timeoutSeconds = 60)
    {
        Stopwatch elapsed = Stopwatch.StartNew();
        while (elapsed.Elapsed < TimeSpan.FromSeconds(timeoutSeconds))
        {
            OrchestrationState? state = await Client.GetOrchestrationStateAsync(instanceId)
                .WaitAsync(TimeSpan.FromSeconds(15), cancellation);
            transcript.Add(new
            {
                instanceId,
                status = state?.OrchestrationStatus.ToString(),
                executionId = state?.OrchestrationInstance.ExecutionId,
                input = state?.Input,
                output = state?.Output,
                customStatus = state?.Status,
                tags = state?.Tags,
            });
            if (state?.OrchestrationStatus == OrchestrationStatus.Failed)
                throw new InvalidOperationException($"Workflow {instanceId} failed: {state.Output}");
            if (state is not null && predicate(state)) return state;
            await Task.Delay(100, cancellation);
        }
        throw new TimeoutException($"Workflow {instanceId} did not reach the expected state");
    }

    public Task<OrchestrationState> WaitForStatusAsync(string instanceId, string status) =>
        StateWhenAsync(instanceId, state => !string.IsNullOrEmpty(state.Status) && Read<string>(state.Status) == status);

    public async Task SaveResultAsync(object result) =>
        await File.WriteAllTextAsync(Path.Combine(ArtifactDirectory, "result.json"),
            JsonSerializer.Serialize(result, JsonOptions) + "\n", cancellation);

    public static T Read<T>(string json) =>
        JsonSerializer.Deserialize<T>(json) ?? throw new InvalidOperationException($"Expected {typeof(T).Name}");

    public static void Require(bool condition, string message)
    {
        if (!condition) throw new InvalidOperationException(message);
    }

    internal async Task DrainAsync(StreamReader reader, string file, Action<string>? observed = null)
    {
        await using StreamWriter log = new(Path.Combine(ArtifactDirectory, file), append: true);
        log.AutoFlush = true;
        while (await reader.ReadLineAsync() is { } line)
        {
            await log.WriteLineAsync(line);
            observed?.Invoke(line);
        }
    }

    internal static async Task StopAsync(Process process)
    {
        if (process.HasExited) return;
        process.StandardInput.Close();
        try
        {
            await process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(15));
        }
        catch (TimeoutException)
        {
            if (!process.HasExited) process.Kill(entireProcessTree: true);
            await process.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(5));
        }
    }

    internal static ProcessStartInfo StartInfo(string executable, IEnumerable<string> arguments)
    {
        ProcessStartInfo start = new(executable)
        {
            UseShellExecute = false,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        foreach (string argument in arguments) start.ArgumentList.Add(argument);
        return start;
    }

    static async Task<string> RunAsync(string executable, string[] arguments, TimeSpan timeout,
        CancellationToken cancellation, bool includeStandardError = false,
        IReadOnlyDictionary<string, string>? environment = null)
    {
        ProcessStartInfo start = StartInfo(executable, arguments);
        if (environment is not null)
            foreach ((string name, string value) in environment) start.Environment[name] = value;
        using Process process = Process.Start(start)
            ?? throw new InvalidOperationException($"Could not start {executable}");
        Task<string> stdout = process.StandardOutput.ReadToEndAsync();
        Task<string> stderr = process.StandardError.ReadToEndAsync();
        try
        {
            await process.WaitForExitAsync(cancellation).WaitAsync(timeout, cancellation);
        }
        finally
        {
            if (!process.HasExited)
            {
                process.Kill(entireProcessTree: true);
                await process.WaitForExitAsync();
            }
        }
        string output = await stdout;
        string error = await stderr;
        Require(process.ExitCode == 0, $"{executable} {string.Join(' ', arguments)} failed ({process.ExitCode}):\n{error}\n{output}");
        return includeStandardError ? output + error : output;
    }

    static string FindRoot()
    {
        for (DirectoryInfo? directory = new(AppContext.BaseDirectory); directory is not null; directory = directory.Parent)
        {
            if (File.Exists(Path.Combine(directory.FullName, "reaction.json"))
                && File.Exists(Path.Combine(directory.FullName, "Cargo.toml")))
                return directory.FullName;
            string example = Path.Combine(directory.FullName, "examples", "lib", "durable-task");
            if (File.Exists(Path.Combine(example, "DurableTaskExample.csproj")))
                return example;
        }
        throw new DirectoryNotFoundException("Cannot find the example's reaction.json and Cargo.toml");
    }

    public async ValueTask DisposeAsync()
    {
        try
        {
            await File.WriteAllTextAsync(Path.Combine(ArtifactDirectory, "transcript.json"),
                JsonSerializer.Serialize(new { drasi = transcript, postgres = database?.Writes }, JsonOptions) + "\n");
        }
        finally
        {
            try
            {
                await StopDrasiAsync();
            }
            finally
            {
                service?.Dispose();
                logging.Dispose();
                try
                {
                    if (database is not null) await database.DisposeAsync();
                }
                finally
                {
                    await Task.WhenAll(CleanupContainerAsync(container, "emulator"),
                        CleanupContainerAsync(postgresContainer, "postgres"));
                }
            }

            async Task CleanupContainerAsync(string? name, string logPrefix)
            {
                if (name is null) return;
                string existing = await RunAsync("docker", ["ps", "-aq", "--filter", $"name=^/{name}$"],
                    TimeSpan.FromSeconds(20), CancellationToken.None);
                if (string.IsNullOrWhiteSpace(existing)) return;
                try
                {
                    string state = await RunAsync("docker", ["inspect", "--format", "{{json .State}}", name],
                        TimeSpan.FromSeconds(20), CancellationToken.None);
                    await File.WriteAllTextAsync(Path.Combine(ArtifactDirectory, logPrefix + "-state.json"), state);
                    string logs = await RunAsync("docker", ["logs", name], TimeSpan.FromSeconds(20),
                        CancellationToken.None, includeStandardError: true);
                    await File.WriteAllTextAsync(Path.Combine(ArtifactDirectory, logPrefix + ".log"), logs);
                }
                finally
                {
                    try
                    {
                        await RunAsync("docker", ["stop", "--time", "5", name], TimeSpan.FromSeconds(30), CancellationToken.None);
                    }
                    finally
                    {
                        await RunAsync("docker", ["rm", "--volumes", name], TimeSpan.FromSeconds(30), CancellationToken.None);
                    }
                }
            }
        }
    }
}
