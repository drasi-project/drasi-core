// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

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
