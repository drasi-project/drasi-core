# Reconstructible historical series

These seven cumulative patches make every one of the 20 commit-specific patches
in [`../index.json`](../index.json) reconstructible from public evidence. Each
series starts from the public GitHub commit
[`43e7d250`](https://github.com/drasi-project/drasi-core/commit/43e7d25034bc8fababf3021b3c6e17d476d42e4a)
and ends at a historical lineage tip.

[`index.json`](index.json) records:

- the exact remote base and historical tip;
- the cumulative patch checksum and changed paths;
- which of the 20 local-only commits each series covers; and
- application verification against a fresh archive fetched from GitHub.

All seven patches passed `git apply --check` and applied from that archive.
The `926919b4` and `a373d466` series retain one historical blank-line
whitespace warning each, but apply successfully. Credential-shaped test fixture
strings were replaced with `test-token-not-a-secret`; the count for each patch
is recorded in the index.

To reconstruct a lineage:

```bash
curl -L \
  https://api.github.com/repos/drasi-project/drasi-core/tarball/43e7d25034bc8fababf3021b3c6e17d476d42e4a \
  -o drasi-core-base.tar.gz
mkdir reconstructed
tar -xzf drasi-core-base.tar.gz -C reconstructed --strip-components=1
git -C reconstructed apply --check path/to/<remote-base>--<tip>.patch
git -C reconstructed apply path/to/<remote-base>--<tip>.patch
```

These patches preserve historical evidence and preimages. They are not a
recommendation to apply WorkGraph-specific component stacks to current `main`.
