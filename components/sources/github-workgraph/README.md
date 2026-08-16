# GitHub WorkGraph Source

Streams one organization's GitHub webhook into Drasi. Conversion is read-only and payload-only: the source has no GitHub token and makes no REST or GraphQL calls. A separately configured bootstrap provider owns initial materialization; bootstrap and streaming must use the schema and IDs below.

## Configuration

```yaml
id: github-workgraph
kind: github-workgraph
autoStart: true
organization: drasi-project
webhook:
  host: 0.0.0.0
  port: 8080
  path: /webhook
  secret:
    kind: Secret
    name: github-workgraph-webhook-secret
  bodyLimitBytes: 26214400
durability:
  enabled: true
  maxEvents: 10000
  capacityPolicy: RejectIncoming
```

`organization` is one login; there is no repository, Project, token, or API scope. The secret is a `SecretReference`, the path is static, durability uses `RejectIncoming`, and unknown or obsolete fields are rejected. Create one organization webhook using `application/json` and the same secret for `repository`, `issues`, `issue_comment`, `pull_request`, and `pull_request_review`; other families, including inline comments and Projects, are ignored.

## Graph contract

Properties are camelCase. Missing optional fields are omitted; explicit nulls remain null. GitHub IDs are global payload `node_id` values.

| Node | ID | Payload-derived properties |
|---|---|---|
| `GitHubOrganization` | `organization.node_id` | `nodeId`, `databaseId`, `login`, `url`, `avatarUrl`, `description` |
| `GitHubRepository` | `repository.node_id` | identity/name/owner, URL, privacy/archive/fork/visibility, default branch, topics, timestamps |
| `GitHubIssue` | `issue.node_id` | identity/number/title/body/bodyDigest/state/stateReason/lock/timestamps, author, URL, repository, assignees, labels, status |
| `GitHubPullRequest` | `pull_request.node_id` | Issue fields except `stateReason`, plus draft/merge and head/base ref/SHA |
| `GitHubIssueComment`, `GitHubPullRequestComment` | `comment.node_id` | identity/body/timestamps/isEdited, author, URL, repository |
| `GitHubPullRequestReview` | `review.node_id` | identity/state/body/submittedAt/commitId, author, URL, repository |
| `WorkGraphAssignment` | `workgraph-assignment:{organization.node_id}:{encode(assignmentId)}` | typed Assignment fields and source-comment provenance |
| `WorkGraphResult` | `comment.node_id` | typed Result fields and source-comment provenance |
| invalid-comment `WorkGraphError` | `workgraph-error:comment:{comment.node_id}` | error code/message, complete body, comment provenance |
| status-conflict `WorkGraphError` | `workgraph-error:status:{subject.node_id}` | sorted labels and subject/repository provenance |

`bodyDigest` is `sha256:` plus lowercase SHA-256 of exact UTF-8 `body ?? ""`. `encode` leaves ASCII alphanumerics and `-._~` literal and encodes every other UTF-8 byte as uppercase `%HH`. Reviews use `submittedAt`; no timestamp is fabricated.

| Relation and direction | Stable ID |
|---|---|
| Repository `IN_ORGANIZATION` Organization | `IN_ORGANIZATION:{repository}:{organization}` |
| Issue/PR `IN_REPOSITORY` Repository | `IN_REPOSITORY:{item}:{repository}` |
| Comment/Assignment/Result `COMMENT_ON` Issue/PR | `COMMENT_ON:{comment}:{parent}` |
| Review `REVIEW_OF` PR | `REVIEW_OF:{review}:{pr}` |
| Result `RESULT_FOR` Assignment | `RESULT_FOR:{comment}:{assignment_element_id}` |
| Error `ERROR_ON` Issue/PR | invalid: `ERROR_ON:{comment}:{parent}`; status: `ERROR_ON:{error}:{subject}` |

Drasi `in_node` is the relation tail and `out_node` its head. Nodes precede inserted relations; relations precede deleted nodes. Embedded parents in comment/review payloads provide endpoints but are not upserted.

Repository and work-item actions upsert/update/delete their nodes, parent relations, and status state. Comment edits classify `changes.body.from` and the new body: changing Assignment ID replaces its node, while changing a Result's Assignment moves only `RESULT_FOR`. Review submit inserts; edit/dismiss updates.

## WorkGraph comments

Only Issue/PR conversation comments are classified. The canonical Assignment producer emits these exact bytes, including the final LF:

````text
<details>
<summary>WorkGraph Assignment</summary>

WorkGraphAssignment/v1

Brief non-empty human summary.

```json
{
  "assignmentId": "a-42",
  "agentProfile": "validator",
  "priority": 10,
  "taskType": "issue-validation",
  "task": {
    "validationProfile": "default",
    "criteria": [
      "Reproduces"
    ]
  }
}
```
</details>
````

Results use exact summary `<summary>WorkGraph Result</summary>` and marker `WorkGraphResult/v1`; their producer omits the final LF. Assignments require exactly one final LF, Results end exactly at `</details>`, and only LF separators are accepted. The opening tag must be exact `<details>` (no `open` or attributes), every displayed blank line is required, Result human summary must byte-equal payload `summary`, and JSON must exactly equal `serde_json::to_string_pretty` output. Extra whitespace, CRLF, prose, tags, fences, compact JSON, literal `\n` escapes, mismatched families, and unclosed wrappers are invalid marked comments. Every payload rejects unknown fields; the marker supplies the version. Unrelated comments, unrelated `<details>` blocks, and review bodies remain ordinary.

| Type | Strict required JSON |
|---|---|
| Assignment | non-empty `assignmentId`, non-empty `agentProfile`, integer `priority >= 0`, `taskType`, typed `task` |
| validation task | `validationProfile`, non-empty `criteria` array of non-empty strings |
| risk task | `riskProfile`, non-empty `dimensions` array of non-empty strings |
| Result | non-empty `assignmentId`, `taskType`, `outcome` (`succeeded`, `failed`, `blocked`), non-empty `summary`, typed `result` |
| validation result | non-empty `criteria` array of `{criterion, passed, evidence}` |
| risk result | non-empty `dimensions` array of `{dimension, score: 0..=100, rationale}` |

`taskType` is `issue-validation` or `issue-risk-profile`. There is no `assignedBy` or `resultId`. A Result's immutable comment ID is its identity; `RESULT_FOR` is derived without checking Assignment existence or task-type equality. Assignment-ID uniqueness within the organization is a producer contract, not a source lookup. Unmarked comments are ordinary nodes; invalid marked comments become only a deterministic, snapshot-free `WorkGraphError`, with stable envelope, JSON, and typed-payload error codes.

## Status, durability, and limitations

The exact case-sensitive `status:` prefix derives Issue/PR workflow status: zero matches sets `status`/`statusLabel` null and deletes any prior error; one sets suffix/full label and deletes the error; multiple set both null and upsert a deterministic error plus `ERROR_ON`. A missing `labels` array changes none of those fields.

Ingress verifies raw-body `X-Hub-Signature-256`, validates and converts, then serially appends every `SourceChange` to the existing WAL before storing the `X-GitHub-Delivery` dedupe marker and returning `202`. A background dispatcher uses `SourceBase`; replay positions are big-endian `u64`; pruning stops at the minimum confirmed position. Invalid signature is `401`, malformed headers/JSON `400`, organization mismatch `403`, payload shape error `422`, ignored events `204`, and WAL/state/capacity failure `503`.

WAL append is per change, not webhook-transactional. A crash may persist a prefix that redelivery repeats, so consumers must tolerate observable at-least-once changes despite stable IDs. GitHub provides no ordering guarantee or automatic failed-delivery retry. Payload-only deletes/transfers cannot infer unknown descendants; PRs and reviews have no delete action. Completed-delivery dedupe markers grow with deliveries and are retained until deprovisioning.
