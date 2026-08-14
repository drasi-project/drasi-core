# GitHub Source Graph Schema

Element IDs use GitHub global node IDs where available.

Relation IDs are deterministic:

`{RELATION_LABEL}:{out_node_global_id}:{in_node_global_id}`

## Node Labels

### `GitHubRepository`
- ID: repository global ID
- Properties:
  - `nameWithOwner`, `name`, `owner`, `description`, `url`
  - `isArchived`, `isPrivate`
  - `createdAt`, `updatedAt`
  - `defaultBranch`

### `GitHubIssue`
- ID: issue global ID
- Properties:
  - `number`, `title`, `body`, `bodyDigest`, `state`
  - `createdAt`, `updatedAt`, `closedAt`
  - `authorLogin`, `url`
  - `repositoryNameWithOwner`
  - `isEdited`
  - `assignees` (string list)
  - `labels` (string list)

### `GitHubPullRequest`
- ID: pull request global ID
- Properties:
  - `number`, `title`, `body`, `bodyDigest`, `state`
  - `createdAt`, `updatedAt`, `closedAt`, `mergedAt`
  - `authorLogin`, `url`
  - `repositoryNameWithOwner`
  - `isDraft`, `isEdited`
  - `headRefName`, `baseRefName`
  - `assignees` (string list)
  - `labels` (string list)

For `GitHubIssue` and `GitHubPullRequest`, `body` preserves the authoritative
GitHub value. `bodyDigest` is always `sha256:` followed by the lowercase SHA-256
hex digest of the exact UTF-8 bytes of `body ?? ""`, without normalization.
The shared contract vector
`Context\nWorkGraph-Validation: pass\n` produces
`sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa`.

### `GitHubIssueComment`
- ID: issue comment global ID
- Properties:
  - `body`, `createdAt`, `updatedAt`
  - `authorLogin`, `authorId`, `authorDatabaseId`, `authorType`, `url`
  - `isEdited`, `isMinimized`
  - `performedViaGithubAppId`
  - `repositoryNameWithOwner`

### `GitHubPullRequestReview`
- ID: review global ID
- Properties:
  - `state`, `body`
  - `createdAt`, `updatedAt`
  - `authorLogin`, `authorId`, `authorDatabaseId`, `authorType`, `url`
  - `performedViaGithubAppId`

### `GitHubPullRequestReviewComment`
- ID: review comment global ID
- Properties:
  - `body`, `path`, `position`, `line`
  - `createdAt`, `updatedAt`
  - `authorLogin`, `authorId`, `authorDatabaseId`, `authorType`, `url`
  - `isEdited`, `diffHunk`
  - `performedViaGithubAppId`
  - `repositoryNameWithOwner`

### `GitHubProject`
- ID: project global ID
- Properties:
  - `title`, `number`, `url`
  - `createdAt`, `updatedAt`
  - `owner`

### `GitHubProjectItem`
- ID: project item global ID
- Properties:
  - `type`, `createdAt`, `updatedAt`
  - `statusFieldId`, `statusOptionId`, `statusName`
  - `contentType`, `contentId`, `contentNumber`, `contentTitle`, `contentState`, `contentBody`
  - `repositoryNameWithOwner`
  - dynamic project-field projections as `field_<normalized_name>`

## Relation Labels

### `IN_REPOSITORY`
- `(GitHubIssue)-[:IN_REPOSITORY]->(GitHubRepository)` and `(GitHubPullRequest)-[:IN_REPOSITORY]->(GitHubRepository)`
- IDs:
  - `IN_REPOSITORY:{issue_id}:{repository_id}`
  - `IN_REPOSITORY:{pull_request_id}:{repository_id}`

### `COMMENT_ON`
- `(GitHubIssueComment)-[:COMMENT_ON]->(GitHubIssue)` or `(GitHubIssueComment)-[:COMMENT_ON]->(GitHubPullRequest)`
- ID: `COMMENT_ON:{comment_id}:{parent_id}`

### `REVIEW_OF`
- `(GitHubPullRequestReview)-[:REVIEW_OF]->(GitHubPullRequest)`
- ID: `REVIEW_OF:{review_id}:{pull_request_id}`

### `PART_OF_REVIEW`
- `(GitHubPullRequestReviewComment)-[:PART_OF_REVIEW]->(GitHubPullRequestReview)`
- ID: `PART_OF_REVIEW:{review_comment_id}:{review_id}`

### `IN_PROJECT`
- `(GitHubProjectItem)-[:IN_PROJECT]->(GitHubProject)`
- ID: `IN_PROJECT:{project_item_id}:{project_id}`

### `TRACKS`
- `(GitHubProjectItem)-[:TRACKS]->(GitHubIssue)` or `(GitHubProjectItem)-[:TRACKS]->(GitHubPullRequest)`
- ID: `TRACKS:{project_item_id}:{tracked_content_id}`
