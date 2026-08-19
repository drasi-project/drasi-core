# OTel source graph schema

The source flattens Resource attributes onto `Service`. It does not emit Resource nodes.

## Nodes

| Label | Identity | Properties | Lifecycle |
| --- | --- | --- | --- |
| `Service` | `svc:{namespace}/{name}` or `svc:{name}` | `name`, `namespace?` (`service.namespace`), `environment?`, `instanceId?` (`service.instance.id`), `registeredAt`, `lastSeen` | Upsert |
| `Metric` | `metric:{serviceId}:{metric.name}` | `name`, `unit`, `value`, `observedAt`, `receivedAt` | Upsert current value |
| `Heartbeat` | `hb:{serviceId}` | `lastSeen` | Upsert |
| `LogEvent` | `log:{serviceId}:{eventName\|hash}:{nanos}` | `service`, `severity`, `body`, `eventName?`, `observedAt` | Insert + TTL delete |

## Relationships

| Label | From | To | Identity | Lifecycle |
| --- | --- | --- | --- | --- |
| `REPORTS` | Service | Metric | `reports:{metricId}` | Upsert |
| `HEARTBEAT` | Service | Heartbeat | `rel-hb:{serviceId}` | Upsert |
| `DEPENDS_ON` | Service | Service | `dep:{from}:{to}` | Refresh + TTL delete (receipt time) |
| `EMITS` | Service | LogEvent | `emits:{logId}` | Insert + TTL delete (receipt time) |

Cross-source joins such as `RUNS` (Deployment.name → Service.name) and `GOVERNED_BY` (Service.name → policy) are query metadata, not emitted by this source.
