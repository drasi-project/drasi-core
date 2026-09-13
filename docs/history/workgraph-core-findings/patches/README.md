# Portable historical patches

These patches preserve 20 historical implementation commits that remained
readable in a local Git object database but were not reachable from a current
remote branch and did not resolve through the GitHub commit API.

[`index.json`](index.json) records each original commit, its exact parent,
subject, changed files, patch size, and SHA-256 checksum. Patch files contain
only `git diff --binary --full-index <parent> <commit>` output; commit author
headers, local paths, session metadata, credentials, runtime databases, WALs,
and git bundles are excluded.

The patches are evidence, not a stack that should be applied wholesale. Most
belong to removed or superseded WorkGraph-specific components. To inspect or
recover one change in a compatible checkout:

```bash
git apply --check path/to/<commit>.patch
git apply --3way path/to/<commit>.patch
```

Prefer issue-specific extraction of surviving generic behavior. Failed
`--check` on current `main` is expected when the historical component no longer
exists.
