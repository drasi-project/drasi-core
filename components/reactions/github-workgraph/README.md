# GitHub WorkGraph Dispatcher Reaction

The GitHub WorkGraph reaction dispatches tasks from one capacity query to free
worker slots. It preserves the query's ordering and writes the canonical
`WorkGraphTaskLease/v1` mapping as a GitHub issue comment.

## Configuration

The reaction must subscribe to exactly one query.

```yaml
reactions:
  - id: workgraph-dispatcher
    kind: github-workgraph-dispatcher
    queries:
      - workgraph-capacity
    properties:
      token:
        kind: Secret
        name: github-workgraph-token
      apiBaseUrl: https://api.github.com
```

`token` must be able to create issue comments in every task repository.
`apiBaseUrl` is optional and can be changed for GitHub Enterprise Server. The
resolved token is used only for bearer authentication and is not logged.

## Capacity row

The query may return additional fields, but every current row must contain:

```json
{
  "repositoryOwner": "acme",
  "repositoryName": "workgraph",
  "workerId": "validator-1",
  "leaseDurationSeconds": 900,
  "activeLeaseIds": [],
  "freeSlotIds": [
    "validator-1/1",
    "validator-1/2"
  ],
  "dispatchableTasks": [
    {
      "taskNodeId": "I_kwDOExample1",
      "taskNumber": 101,
      "repositoryOwner": "acme",
      "repositoryName": "tasks",
      "assignmentCommentNodeId": "IC_kwDOAssignment1",
      "workerId": "validator-1"
    }
  ]
}
```

The reaction filters slots and tasks already held by its short-lived in-process
pending map, then pairs the remaining `freeSlotIds` and `dispatchableTasks`
positionally without sorting. A pending entry is removed only when a later row
for the same repository and worker includes its exact lease ID in
`activeLeaseIds`.

After a successful issue-comment request, the pending entry prevents a repeated
capacity row from assigning either the same slot or the same task again while
the source and query catch up. Pending state is intentionally not durable.

Malformed rows and unsuccessful GitHub requests put the reaction in an error
state. The reaction does not retry, scan existing comments, reconcile startup
state, or claim exactly-once delivery.
