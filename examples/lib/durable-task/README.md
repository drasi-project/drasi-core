# Watch a database change drive a .NET workflow

Follow an order from placed to paid, shipped, and delivered. Each step writes to
PostgreSQL. Drasi starts a .NET Durable Task workflow, then sends named `paid`,
`shipped`, and `delivered` events to that same workflow.

```text
PostgreSQL -> Drasi query + reaction -> Durable Task Scheduler -> .NET workflow
```

## Run it

From the repository root:

```sh
dotnet run --project examples/lib/durable-task
```

Prerequisites: a running Docker daemon, .NET SDK 10, Rust, `protoc`, Clang, and
CMake. The first build may take several minutes.

The app starts everything. **Press Enter to advance each step**, or `q` / Ctrl+C
to quit. It shows the SQL being executed and waits for the workflow to process it.

**Open the printed `DTS dashboard` URL in your browser.** Select the `default`
task hub. After the first step, open `order-42` and select **History**. Keep the
dashboard beside the terminal and refresh after each step to see the named events
and activities.
The URL uses a free localhost port chosen for this run; no Azure account or login
is needed.

| Step | Database change | Query change | Workflow |
|------|-----------------|--------------|----------|
| Placed | Insert `order-42` with `status='placed'` | ADDED | Starts |
| Paid | Set `status='paid'` | UPDATED | Resumes the `paid` wait |
| Shipped | Set `status='shipped'` | UPDATED | Resumes the `shipped` wait |
| Delivered | Set `status='delivered'` | UPDATED | Resumes the `delivered` wait and completes |

For example, after the second step:

```text
Order: order-42
Status: paid
Query change: UPDATED
Workflow: RUNNING
```

The delivered order stays in the database and the query result. Delivery reaches
the workflow as an UPDATE, just like paid and shipped. The .NET app never sends
these events directly to DTS.

After delivery, the app pauses so you can inspect the completed workflow's
history. **Press Enter or `q` once more to clean up.** The PostgreSQL and DTS
containers are local and disposable; cleanup also closes the dashboard.
Ctrl+C or closed input exits as well. The app prints the location of retained
diagnostics.

## Query

[`query.cypher`](query.cypher) watches each order's status without filtering out
delivered orders:

```cypher
MATCH (o:orders)
RETURN o.order_id AS orderId,
	o.status AS status
```

## Reaction setup

[`reaction.json`](reaction.json) starts `OrderWorkflow` on ADD. On UPDATE,
`eventName: "{{after.status}}"` sends the event named by the new database value:
`paid`, `shipped`, or `delivered`. Each targets the same order instance. Deletion
is ignored in this demo. The app supplies the local DTS connection values.

```json
{
	"endpoint": "${DTS_ENDPOINT}",
	"taskHub": "${DTS_TASKHUB:-default}",
	"producerId": "${DTS_PRODUCER_ID:-orders-example-v1}",
	"authentication": "none",
	"defaultTemplate": {
		"added": {
			"action": "start",
			"orchestration": "OrderWorkflow",
			"instanceId": "{{after.orderId}}",
			"template": "{\"orderId\":{{json after.orderId}},\"status\":{{json after.status}}}"
		},
		"updated": {
			"action": "raiseEvent",
			"instanceId": "{{after.orderId}}",
			"eventName": "{{after.status}}",
			"template": "{\"orderId\":{{json after.orderId}},\"status\":{{json after.status}}}"
		},
		"deleted": {
			"action": "ignore"
		}
	}
}
```

## Workflow

[`dotnet/OrderWorkflow.cs`](dotnet/OrderWorkflow.cs) waits explicitly for payment,
then shipment, then delivery. There is no generic status-change event or loop:

```csharp
using Microsoft.DurableTask;

public sealed class OrderWorkflow : TaskOrchestrator<OrderStatus, string>
{
	public override async Task<string> RunAsync(TaskOrchestrationContext context, OrderStatus order)
	{
		await context.CallActivityAsync(nameof(ReportStatus), order);
		context.SetCustomStatus("placed");

		OrderStatus paid = await context.WaitForExternalEvent<OrderStatus>("paid");
		await context.CallActivityAsync(nameof(ReportStatus), paid);
		context.SetCustomStatus("paid");

		OrderStatus shipped = await context.WaitForExternalEvent<OrderStatus>("shipped");
		await context.CallActivityAsync(nameof(ReportStatus), shipped);
		context.SetCustomStatus("shipped");

		OrderStatus delivered = await context.WaitForExternalEvent<OrderStatus>("delivered");
		await context.CallActivityAsync(nameof(ReportStatus), delivered);
		context.SetCustomStatus("delivered");

		return $"Order {order.orderId} delivered.";
	}
}

public record OrderStatus(string orderId, string status);

public sealed class ReportStatus : TaskActivity<OrderStatus, string>
{
	public override Task<string> RunAsync(TaskActivityContext context, OrderStatus order)
	{
		Console.WriteLine($"Order {order.orderId}: {order.status}");
		return Task.FromResult(order.status);
	}
}
```

`ReportStatus` writes to the console; it does not charge a card or arrange shipping.
Each wait requires its named event. An early `delivered` event cannot skip the
payment or shipment waits; the SDK buffers it until the workflow reaches delivery.

The walkthrough itself is in [`dotnet/Program.cs`](dotnet/Program.cs). Process
management and database plumbing live under `dotnet/infrastructure/`.

This is a local learning demo. Repeated delivery can print the same report again;
it is not an exactly-once notification service.
