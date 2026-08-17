# GitHub WorkGraph Source

Streams one organization's GitHub webhook into Drasi. Conversion is read-only
and payload-only: the source has no GitHub token and makes no REST or GraphQL
calls. A separately configured bootstrap provider owns initial materialization;
bootstrap and streaming must use the schema and IDs below.

## Configuration

```yaml
id: github-workgraph
kind: github-workgraph
autoStart: true
organization: drasi-project
repositories:
  - drasi-workgraph-demo
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

`organization` is one login. `repositories` is optional: omitted or empty means
all repositories in that organization. Each entry is either a bare repository
name (`drasi-workgraph-demo`) or a full `owner/name`; a full owner must match
`organization` case-insensitively. Names are case-insensitive, limited to 100
ASCII letters, digits, `.`, `-`, or `_`, and normalized to sorted, deduplicated
lowercase bare names. Malformed entries and foreign owners are rejected. The
secret is a `SecretReference`, the path is static, durability uses
`RejectIncoming`, and unknown or obsolete fields are rejected.

Create one organization webhook using `application/json` and the same secret for
`repository`, `issues`, `issue_comment`, `pull_request`, and
`pull_request_review`. Other families, including inline comments and Projects,
are ignored. Filtering is stateless: each supported delivery carries its
repository metadata, so an excluded repository returns `204` before any WAL or
delivery-dedupe write. Transfers and repository renames use the old/new
repository metadata in that delivery to emit only the in-scope side. The Source
does not call GitHub or maintain a repository cache.

## Graph contract

Properties are camelCase. Missing optional fields are omitted; explicit nulls
remain null. GitHub IDs are global payload `node_id` values.

| Node | ID | Payload-derived properties |
|---|---|---|
| `GitHubOrganization` | `organization.node_id` | `nodeId`, `databaseId`, `login`, `url`, `avatarUrl`, `description` |
| `GitHubRepository` | `repository.node_id` | identity/name/owner, URL, privacy/archive/fork/visibility, default branch, topics, timestamps |
| `GitHubIssue` | `issue.node_id` | identity/number/title/body/bodyDigest/state/stateReason/lock/timestamps, author, URL, repository, assignees, labels, labelDetails, status/statusLabel |
| `GitHubPullRequest` | `pull_request.node_id` | Issue fields except `stateReason`, plus draft/merge and head/base ref/SHA |
| `GitHubIssueComment`, `GitHubPullRequestComment` | `comment.node_id` | identity/body/timestamps/isEdited, author, URL, repository |
| `GitHubPullRequestReview` | `review.node_id` | identity/state/body/submittedAt/commitId, author, URL, repository |
| `WorkGraphAssignment` | `workgraph-assignment:{organization.node_id}:{encode(assignmentId)}` | typed Assignment fields and source-comment provenance |
| `WorkGraphResult` | `comment.node_id` | typed Result fields and source-comment provenance |
| invalid-comment `WorkGraphError` | `workgraph-error:comment:{comment.node_id}` | error code/message, complete body, comment provenance |
| status-conflict `WorkGraphError` | `workgraph-error:status:{subject.node_id}` | sorted labels and subject/repository provenance |

`bodyDigest` is `sha256:` plus lowercase SHA-256 of exact UTF-8 `body ?? ""`.
`encode` leaves ASCII alphanumerics and `-._~` literal and encodes every other
UTF-8 byte as uppercase `%HH`. Reviews use `submittedAt`; no timestamp is
fabricated.

`labels` remains the ordered list of label names. `labelDetails` is the same
ordered list with generic paired GitHub identity:

```json
[
  {
    "name": "bug",
    "nodeId": "LA_kwDO..."
  }
]
```

The list-of-map value is directly consumable by Drasi Cypher. To preserve every
current non-status label and prepend the configured `status:awaitingValidation`
GraphQL node ID, use:

```cypher
coll.insert(
  [label IN issue.labelDetails
   WHERE NOT (label.name STARTS WITH 'status:')
   | label.nodeId],
  0,
  $awaitingValidationLabelId
)
```

`coll.insert` is used because the supported dialect does not concatenate lists
with `+`. Webhook Issue/PR label objects already carry both fields, so this adds
no API call or cache. A present `labels` array is rejected as a payload-shape
error unless every entry has a nonempty string `name` and `node_id`; omitting the
whole field on a partial update still leaves all label-derived properties
unchanged.

| Relation and direction | Stable ID |
|---|---|
| Repository `IN_ORGANIZATION` Organization | `IN_ORGANIZATION:{repository}:{organization}` |
| Issue/PR `IN_REPOSITORY` Repository | `IN_REPOSITORY:{item}:{repository}` |
| Comment/Assignment/Result `COMMENT_ON` Issue/PR | `COMMENT_ON:{comment}:{parent}` |
| Review `REVIEW_OF` PR | `REVIEW_OF:{review}:{pr}` |
| Result `RESULT_FOR` Assignment | `RESULT_FOR:{comment}:{assignment_element_id}` |
| Error `ERROR_ON` Issue/PR | invalid: `ERROR_ON:{comment}:{parent}`; status: `ERROR_ON:{error}:{subject}` |

Drasi `in_node` is the relation tail and `out_node` its head. Nodes precede
inserted relations; relations precede deleted nodes. Embedded parents in
comment/review payloads provide endpoints but are not upserted.

Repository and work-item actions upsert/update/delete their nodes, parent
relations, and status state. Comment edits classify `changes.body.from` and the
new body: changing Assignment ID replaces its node, while changing a Result's
Assignment moves only `RESULT_FOR`. Review submit inserts; edit/dismiss updates.

## WorkGraph comments

Only Issue/PR conversation comments are classified. The envelope is exact:

````markdown
<details>
<summary>WorkGraph Assignment</summary>

WorkGraphAssignment/v1

Validate the synthetic fixture Issue.

```json
{
  "assignmentId": "fixture-701-validation",
  "agentProfile": "issue-validator",
  "priority": 10,
  "taskType": "issue-validation",
  "task": {
    "validationProfile": "new-issue-default"
  }
}
```
</details>
````

Result comments use `<summary>WorkGraph Result</summary>` and
`WorkGraphResult/v1`. The opening tag is literal `<details>` with no attributes,
so GitHub collapses the body by default. Every separator is LF: there is one LF
after the opening and summary lines, one blank line after the summary label,
marker, and one-line non-empty human summary, and exactly one final LF after
`</details>`.

The fenced object must equal the canonical two-space serialization of the typed
payload, including field order. CRLF, literal `\n` separators, compact JSON,
additional or mismatched fences, wrapper attributes, missing or extra blank
lines, prose outside the wrapper, and missing or extra final LFs are invalid.
The Result human summary and typed `summary` field must not contain
`WorkGraphResult/v1`, even as a substring, and they must byte-equal each other.
Every object rejects unknown fields; the marker supplies the version.

| Type | Strict required JSON |
|---|---|
| Assignment | non-empty `assignmentId`, non-empty `agentProfile`, integer `priority >= 0`, `taskType`, typed `task` |
| validation task | one non-empty `validationProfile`; criteria resolve from `.github/workgraph/profiles/issue-validation/<validationProfile>.md` |
| risk task | `riskProfile`, non-empty `dimensions` array of non-empty strings |
| Result | non-empty `assignmentId`, `taskType`, `outcome` (`succeeded`, `failed`, `blocked`), non-empty `summary`, typed `result` |
| validation result | non-empty `criteria` array of `{criterion, passed, evidence}` |
| risk result | non-empty `dimensions` array of `{dimension, score: 0..=100, rationale}` |

An issue-validation Assignment carries only the profile name. Core does not
resolve repository profile files; the agent/reporter reads
`.github/workgraph/profiles/issue-validation/<validationProfile>.md`. A legacy
`task.criteria` field is rejected as unknown. Result criterion entries are
unchanged.

`taskType` is `issue-validation` or `issue-risk-profile`. There is no
`assignedBy` or `resultId`. A Result's immutable comment ID is its identity;
`RESULT_FOR` is derived without checking Assignment existence or task-type
equality. Assignment-ID uniqueness within the organization is a producer
contract, not a source lookup.

Unmarked comments are ordinary nodes. Invalid marked comments become only a
deterministic, snapshot-free `WorkGraphError`, with stable envelope, JSON, and
typed-payload error codes.

## Status, durability, and limitations

The exact case-sensitive `status:` prefix derives Issue/PR workflow status:
zero matches sets `status`/`statusLabel` null and deletes any prior error; one
sets suffix/full label and deletes the error; multiple set both null and upsert
a deterministic error plus `ERROR_ON`. A missing `labels` array changes none of
`labels`, `labelDetails`, `status`, or `statusLabel`.

Ingress verifies raw-body `X-Hub-Signature-256`, validates and converts, then
serially appends every `SourceChange` to the existing WAL before storing the
`X-GitHub-Delivery` dedupe marker and returning `202`. A background dispatcher
uses `SourceBase`; replay positions are big-endian `u64`; pruning stops at the
minimum confirmed position. Invalid signature is `401`, malformed headers/JSON
`400`, organization mismatch `403`, payload shape error `422`, ignored events
`204`, and WAL/state/capacity failure `503`.

WAL append is per change, not webhook-transactional. A crash may persist a
prefix that redelivery repeats, so consumers must tolerate observable
at-least-once changes despite stable IDs. GitHub provides no ordering guarantee
or automatic failed-delivery retry. Payload-only deletes/transfers cannot infer
unknown descendants; PRs and reviews have no delete action. Completed-delivery
dedupe markers grow with deliveries and are retained until deprovisioning.
