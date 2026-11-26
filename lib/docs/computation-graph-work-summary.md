# Computation graph work: summary

## Summary

All work is local and uncommitted on `agentofreality-parallel-computation-graph-recovery`,
in drasi-core and drasi-server. learning-drasi-server and test-infra are unchanged.
The fallback branch `agentofreality-native-bootstrap-chain` is pushed in all four repos.

**drasi-core, completed**

1. **Bootstrap chaining.** A component reads its upstream's current state, then follows
   live changes without a gap or duplicate. This covers start in any order, a late first
   input, restart replay, rebuild after a history gap, and crash recovery.
2. **Queries are transformers.** One shared stateful-transformer runtime runs the query,
   the stateful middleware, the view and geometry.
3. **View and geometry ported** to that runtime. Their state is durable, and they
   bootstrap, follow, replay and rebuild exactly as the query does.

**drasi-server, completed**

4. **Storage for any stateful transformer.** A new `nativeProcessing` resource. Before
   this, only queries could be given storage in a server config file.
5. **The server no longer invents connections.** It used to add a hidden start-up
   connection beside each declared connection, and the redesigned graph rejected those.

**Open decisions**

- The drasi-server `bootstrapProvider` shorthand still creates one connection itself.
- A drasi-server graph field named `enableBootstrap` was removed. It is not DrasiLib's
  query `enableBootstrap` (see [Section 3](#3-the-two-enablebootstrap-settings)); it
  might instead be mapped onto DrasiLib's own setting.

**Remaining work:** port policy and optimization. Then finish the remaining bootstrap
items and triage the failing tests listed in [Section 5](#5-failing-tests-to-triage).

---

## 1. drasi-core

### Completed earlier in this branch

| Area | What it does |
|---|---|
| Bootstrap chaining | Graph-owned current state plus follow. Components start in any order. Slow subscribers don't block others. Durable reconnect, replay, rebuild for a running dependent, and crash-safe rebuild. Start-up connections are allowed only from bootstrap providers; everything else initializes over its data connection. |
| Queries as transformers | `StatefulTransformer` in the plugin SDK owns lifecycle, storage, bootstrap, follow, replay, rebuild and delivery. The query plugin and the stateful middleware run on it. |

### Completed in this round

| Item | Detail |
|---|---|
| View on the runtime | Its inputs, and the outputs each input produced, are kept in host storage. It never re-runs mappings to recover old outputs. |
| Geometry on the runtime | Inputs, set memberships and emitted records are kept in host storage. Members, shapes and the pair index are rebuilt in memory when it starts. In-place limit updates are checked against the retained state. |
| Shared SDK helpers | Durable input store and private tables (`HostState`): an input filter, and reading a whole table. Shared update rules: shared options are fixed, retention only grows, and a reconstruction re-bootstraps when bootstrap is configured. The query now uses these rules too. |
| Docs | View and geometry READMEs, the transformer guide, the developer and maintainer guides, and the bootstrap design doc. The design doc wrongly listed `arithmetic` as keeping a dataset; that is corrected. |

**Behaviour changes, now the same for view, geometry and the query:**

- An input batch that changes nothing publishes no output. Input progress still advances.
- State and output are durable with persistent storage. With in-memory storage the
  transformer must set `volatile: true`.

**Evidence:**

- Unit tests: SDK 149, view 21, geometry 18, query 7, middleware 4, middleware-common 8.
- Loaded plugin suite: 184 passed. The 6 ignored tests are process-exit helpers run by
  other tests.
- View and geometry each pass in the middle of a query chain: bootstrap, replay, gap
  rebuild for a running dependent, refusal when a reset would strand a dependent, first
  input after a subscriber, both crash cases, and bootstrap from a SQLite provider.
- Clippy and fmt are clean.

## 2. drasi-server

| Item | Detail |
|---|---|
| `nativeProcessing` resource | `{kind: nativeProcessing, indexes: <index resource>}`. It gives a loaded stateful transformer storage in an existing index resource and declares that resource's durability. Bind it as the transformer's `processing` resource. With `memoryIndexes`, the transformer must set `volatile: true`; otherwise it is refused at start. |
| No invented connections | The readable graph format no longer adds a hidden start-up connection beside each data connection. DrasiLib decides whether a data connection also carries initial state. |
| Removed graph field | The readable graph's per-component `enableBootstrap`, which only switched those hidden connections. A config that still uses it gets a "has been removed" message. |
| `move-a-wall` example | Geometry is bound to a `nativeProcessing` resource. The graph now validates. |
| Test updates | Expectations updated for the middleware plugin's second factory (stateful middleware), and for the removed hidden connections and field. |
| Docs | README and `docs/plugin-architecture.md`. |

**Evidence:**

- New server tests: 2 pass (a stateful transformer runs on `nativeProcessing`, and a
  volatile/durable mismatch is refused).
- Configuration 14/14, OpenAPI 13/13, native computation 52/53, query catalog 9/9,
  recipes 4/4.
- Clippy and fmt are clean.

## 3. The two `enableBootstrap` settings

| Setting | Where | Status |
|---|---|---|
| Query `enableBootstrap` (`QueryConfig.enable_bootstrap`, default on) | DrasiLib | Unchanged |
| Per-connection `initial_state` (`RelationshipPolicy`) | DrasiLib | Unchanged |
| Readable graph component `enableBootstrap` | drasi-server's graph config format | **Removed**: its only effect was adding the hidden connections |

**Decision needed:** should the drasi-server field come back, mapped onto DrasiLib's own
per-component or per-connection setting, rather than removed?

## 4. Open decisions and remaining work

**Decisions**

1. **`bootstrapProvider` shorthand (drasi-server).** It creates the provider-to-component
   connection, and the inline form also creates the provider component. Options: remove
   it, keep it, or move it into DrasiLib.
2. **Graph component `enableBootstrap` (drasi-server).** Leave it removed, or map it onto
   DrasiLib's setting ([Section 3](#3-the-two-enablebootstrap-settings)).

**Remaining work (drasi-core)**

| Item | Note |
|---|---|
| Port `policy` | Same pattern as geometry |
| Port `optimization` | Largest. Its background solves go through the runtime's scheduled-work hooks. |
| Projected reads for filtered and expanded images | `arithmetic` belongs here; it keeps no dataset |
| Rebuild beyond one bounded input | Changed epochs, changed configuration, larger staging |
| Remaining release gates | G3, G4, G6, G8, G9 |

**Deferred:** removing DrasiLib's built-in query and middleware, and all legacy plugin
work.

## 5. Failing tests to triage

None of these come from this round's changes. Most predate it and were hidden by an
assertion that the middleware plugin's new second factory broke.

| Repo | Failure | Likely cause |
|---|---|---|
| drasi-core | 5 shared-graph recovery cases in lib-integration-tests, on the query plugin | Recorded earlier; not yet investigated |
| drasi-server | Retiring a bootstrap provider doesn't converge | Earlier bootstrap redesign |
| drasi-server | A unit test expects a disconnected component to wait for its connections | The redesign lets components start without inputs |
| drasi-server | 3 example configs fail validation | They use an unsupported configuration version |
| drasi-server | `move-a-wall` scene plugin and its tests don't compile | SDK interface changes made before this branch |
