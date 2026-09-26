# GPU Cluster Lab: presenter runbook

Companion to the [implementation guide](gpu-cluster-demo.md). These are scripts and fixture contracts to implement, not claims that the demo is already running. Button names below define the intended UI.

## Presentation order

| Order | Demo | Main point | Suggested time |
|---|---|---|---|
| 1 | Live load redistribution and explanation | A database edit causes a measured change, a decision, and a closed-loop response | 2 minutes |
| 2 | Memory fragmentation | Enough aggregate memory does not imply a valid placement; rearrangement avoids buying hardware | 2 minutes |
| 3 | Healthy but not resilient | Hypothetical worker-loss checks reveal risk before a failure; added capacity restores protection | 3 minutes |
| 4 | A GPU stops reporting | Time passing, without a new report, changes a query result | 1 minute |
| 5 | Worker failure and insufficient capacity | Real simulated execution loss, recovery, explicit infeasibility, restoration | 2 minutes |
| 6 | Direct SQL and browser reconnect | The database and CQs drive the UI; the browser is not the controller | Optional 2 minutes |
| 7 | Regional recovery inside a data boundary | Healthy spare GPUs cannot override a customer's processing-location policy | 3 minutes |
| 8 | A running placement becomes prohibited | Policy edits trigger acknowledged fencing, even without replacement capacity | 2 minutes |

Decision explanations are demonstrated throughout; they are not a disconnected extra screen. For a shorter infrastructure presentation use 1-4; for an infrastructure/security audience use 1, 2, 7, and 8. The original scripts remain valid. All presets use the same application, database schema, policy transformer, queries, solver, and UI. Show one sentence of technical explanation per transition, not a tour of every field.

## Preparation and reset

For a first installation, follow [installation and operation](gpu-cluster-demo.md#11-installation-and-operation) in the guide. Once the documented launcher is implemented, use this preparation sequence from the demo directory:

```sh
./demo up
./demo wait --timeout 120
./demo check --smoke
./demo reset baseline
./demo wait --timeout 120
```

Open **http://127.0.0.1:8090**, or the URL printed by the launcher. Keep `./demo logs drasi` available in another terminal. These are the launcher's required interface, not commands available in this documentation-only workspace.

1. Confirm PostgreSQL, the control service/UI, and the compatible Drasi server hosting the computation graph are running.
2. Confirm instance `gpu-demo`, all nine `ui-*` queries (including `ui-clusters` and `ui-policy`), the plan-writing reaction, and SSE reaction `gpu-demo-ui` are running. The startup tooling creates these; React does not.
3. Open the fleet view with the **Services**, **Resilience**, and **Decisions** panels visible.
4. Select **Load baseline**. This is a deliberate local fixture reset, not a scheduling action.
5. Wait for the UI's query-backed **Scenario ready** state: expected database revisions observed, a complete current policy assessment, expected healthy-device samples fresh, desired/applied placement aligned when feasible, and resilience current. Also check each query is non-stale with an accepted baseline; an open SSE socket alone is insufficient.
6. Keep model profiles and telemetry timing fixed unless the script says otherwise. Do not continue through a red command/query/reaction error.

Reset through the controlled fixture loader, never by manually editing active placements during a demonstration. **Load baseline**, **Load fragmentation**, and **Load regional boundary** use the same coordinator as `./demo reset baseline`, `./demo reset fragmentation`, and `./demo reset regional-boundary`. It gates old writes, stops/reinitializes the owned graph, commits the fixture, and bootstraps fresh observations and policy decisions with monotonic plan versions. Wait for React resynchronization before presenting.

**Scenario ready** means the current scenario is initialized and its result is current; it does not require every resilience scenario to pass. During an intentional infeasibility demonstration, a current **infeasible** result is valid demo state, but it must never be labeled placement **converged**. Suggested presentation times are not recovery-time claims.

**Shared hardware:** two H100 NVL GPUs per worker, each with an 80-GiB workload budget, reference compute capacity 100, planning ceiling 85, background demand 10, background memory zero, and one-second reporting.

**Original fixtures:** workers `inference-a`, `inference-b`, `inference-c` in cluster `eu-primary`, region `westeurope`. Both fixtures bind workloads to synthetic data profile `demo-open`, policy `demo-permissive`, and purpose `demo`. The new-workload form preselects these explicit values; policy evaluation is never bypassed. In scripts 1-6, a passing resilience assessment means the **worker-loss** badge. Loss of the only region is correctly infeasible and has a separate badge.

### Baseline fixture

Use the four model profiles from the guide, each with two replicas spread across workers:

| Worker | Slot 0 | Slot 1 |
|---|---|---|
| `inference-a` | Assistant-0: 76 GiB / 55 demand | Chat-0: 24 GiB / 30 demand |
| `inference-b` | Assistant-1: 76 GiB / 55 demand | Embeddings-0 + Reranker-0: 8 GiB / 45 demand |
| `inference-c` | Chat-1 + Reranker-1: 28 GiB / 55 demand | Embeddings-1: 4 GiB / 20 demand |

The loader establishes this valid layout and labels its initial decision **Fixture setup**, not "solver optimized." Normal subsequent changes always use the actual solver.

### Fragmentation fixture

Use three separately deployed chat services, `chat-alpha`, `chat-bravo`, and `chat-charlie`, with two replicas each. All use the guide's 24-GiB / 30-demand Llama profile. Distinct services represent separately configured endpoints; their replicas still require worker separation.

| Worker | Slot 0 | Slot 1 |
|---|---|---|
| `inference-a` | Alpha-0 | Bravo-0 |
| `inference-b` | Alpha-1 | Charlie-0 |
| `inference-c` | Bravo-1 | Charlie-1 |

No assistant, embedding, or reranking workload exists in this fixture initially. Each GPU has 56 GiB unallocated; the fleet has 336 GiB unallocated in total.

### Regional-boundary fixture

Reuse the baseline's eight replicas and initial assignments, with four additional idle workers:

| Cluster | Region | Workers | GPUs |
|---|---|---|---:|
| `eu-primary` | `westeurope` | `inference-a`, `inference-b`, `inference-c` | 6 |
| `eu-recovery` | `northeurope` | `recovery-a`, `recovery-b` | 4 |
| `us-spare` | `eastus` | `us-a`, `us-b` | 4 |

All eight replicas use data profile `customer-eu-documents`, purpose `customer-support`, and policy `customer-eu-processing`. Allowed regions are `["westeurope","northeurope"]`. Its authority is a fictional customer/company requirement, not a blanket statement about GDPR.

Expected initial state: seven workers, fourteen healthy GPUs, eight ready replicas in West Europe, and empty North Europe/US GPUs. All seven worker-loss and three region-loss cases are feasible within policy. The fixture loader reuses shared catalogs; it does not introduce another graph or hard-coded regional placement algorithm.

The required customer-data access paths and model profiles are ready in both permitted regions. The demonstration controls processing locality, not all replicas, logs, caches, or backups of customer data.

## Demo 1: live load redistribution and a factual explanation

**Start:** Load baseline; wait for Scenario ready.

1. Point out the two separate columns: **configured background demand** and **observed demand**. Show eight ready replicas and a current passing resilience assessment.
2. Select `inference-a / GPU 0`. Set **Background demand** from **10 to 35**, then **Apply**.
3. Show the command acknowledgement first; the displayed saved value updates only when `ui-gpus` receives the CQ change. Then the generator's next report shows changed demand.
4. The assistant needs 55 units, but the new background-adjusted budget is only 50. Wait for a candidate, committed plan, applied allocation, and fresh measurements. At least the assistant must leave this GPU; with this fixture, a one-existing-replica move is feasible.
5. Open the resulting decision. Expected evidence: background demand changed; old budget 50 versus reservation 55; target memory/demand fit; the other assistant replica is on a different worker. Show the actual movement count, not a hard-coded target.
6. Restore background demand to **10**. Wait for fresh measurements and a current assessment. Existing healthy assignments should stay put rather than move back merely to restore the old picture.

**Say:** "The UI changed one database row. Queries, simulation, policy evaluation, and placement converted that into a decision, a database write, and changed measurements."

**End condition:** Current requirements satisfied, applied/desired versions aligned, fresh measurements. Reload baseline before another script that requires its exact starting placement.

## Demo 2: enough memory, but nowhere to put the model

**Start:** Select **Load fragmentation**; wait for Scenario ready.

1. Show **336 GiB free across the fleet**, but **56 GiB is the largest single-GPU gap**. Every device is healthy.
2. Choose **Add workload**. Name it `document-assistant`; select `assistant-v1`, the 76-GiB / 55-demand Qwen profile; request **one replica**. Keep worker separation enabled.
3. Save. Explain that the new replica cannot fit on any GPU without rearranging existing work, even though the cluster has ample aggregate memory.
4. Watch the solver move one chat replica, then place the assistant on the freed GPU. One valid result moves Alpha-0 from `a/0` to `c/0` beside Bravo-1, then places Assistant-0 on `a/0`. Other one-move solutions are equally valid.
5. Open the decision. It must preserve the pre-plan 336/56-GiB figures, show the 76-GiB requirement, and report **one existing replica moved, one new replica placed, zero workers added**.
6. Inspect the destination: two chats reserve 48 GiB / 60 units; the assistant reserves 76 GiB / 55 units on its own GPU. Replica separation remains valid.

**Say:** "This is a placement problem, not simply a shortage alarm. A small rearrangement avoids another GPU worker."

**End condition:** Seven replicas ready on the original six GPUs. Do not require a green resilience badge: serving successfully and surviving the next failure are different claims. The next demo makes that distinction explicit.

## Demo 3: healthy now, unsafe after a failure

**Start:** Reload baseline; wait for all three worker-loss scenarios to pass.

1. Show **8 of 8 replicas ready** and the three read-only questions: could we recover after losing `inference-a`, `inference-b`, or `inference-c`?
2. Add workload `tenant-chat`, selecting `chat-v1`, the 24-GiB / 30-demand Llama profile with **two replicas**, on different workers.
3. Wait for actual placement and fresh reports: **10 of 10 replicas ready**. Current managed demand is now 320 units; no worker has failed.
4. Wait for the resilience batch for the same scheduling signature. All three worker-loss scenarios must fail: losing a worker leaves only 300 assignable units. The UI should say **Current service healthy; single-worker recovery capacity insufficient**.
5. Expand a scenario's evidence. Show the 320-versus-300 constraint and clearly label it **Hypothetical: no configuration or placement writes performed by this check**.
6. Select **Register ready worker**, name it `inference-d`, select cluster `eu-primary`, and choose `h100-nvl-pair-v1`, the same two-GPU hardware profile. This registers an already-provisioned worker; do not describe it as instantaneous Azure VM provisioning.
7. Wait for both GPUs' first fresh reports and a complete current resilience batch. Losing any one of four workers now leaves six GPUs and a feasible placement; the badge returns to passing.
8. Explain that the new worker may remain idle: minimum-movement placement need not relocate healthy work merely to consume spare capacity.

**Say:** "Everything can be green operationally while the next failure would break our deployment requirements. The graph keeps that future question answered."

**End condition:** Ten replicas ready, four healthy workers, and every evaluated additional-worker-loss scenario passing. Provisioning delays/quota failures are a later lifecycle extension, not hidden in this registration action.

## Demo 4: detect an absence, not an explicit failure event

**Start:** Reload baseline; select `inference-a / GPU 0`.

1. Switch **Reporting** off, leaving the device's simulated execution on.
2. Show the distinction: configured reporting is off, but Drasi still has the last fresh observation. Do not immediately paint observed health as failed.
3. Watch the last-report age. Once five seconds have elapsed since that report, the missing-heartbeat query marks the GPU unreachable without another GPU report arriving.
4. Wait for redistribution. Open its decision and show `device-unreachable`, the affected GPU, target fit, and worker separation.
5. Turn reporting on again. The next report restores eligibility. Healthy replicas should not automatically move back.

**Say:** "The trigger was time passing without a report. Neither the UI nor the database wrote a 'GPU failed' event."

**End condition:** Fresh samples, satisfied requirements, and current assessments. Transport/SSE heartbeat messages must not count as GPU heartbeats.

## Demo 5: worker failure, explicit infeasibility, and recovery

**Start:** Reload baseline.

1. Choose **Fail worker** on `inference-a`. Both GPUs stop simulated execution and reporting.
2. Show reduced actual replica availability immediately where appropriate, while observed GPU health waits for its report deadline. The placement solver must not read the power switches directly.
3. After health expiry, wait for recovery on `inference-b` and `inference-c`. All eight replicas should run again, with each service on two different workers.
4. Inspect the plan and explanation. One valid packing on each surviving worker is Assistant + Embeddings on one GPU (80 GiB / 75 units), and Chat + Reranker on the other (28 GiB / 55 units). The solver may choose a different valid layout.
5. Fail `inference-b` as well. After expiry, show **infeasible**: remaining assignable demand capacity is 150 versus 260 required, and only one worker remains for two-replica separation.
6. Confirm no empty or partial success-shaped plan overwrote the last committed plan. Surviving execution and missing replicas stay visible.
7. Recover `inference-b`. Wait for telemetry, reapplication/replanning as needed, and convergence on two workers.
8. Recover `inference-a`; wait for a current passing single-worker-loss assessment. Again, spare capacity need not trigger gratuitous moves.

**Say:** "The controller can repair a feasible situation, but it does not invent capacity or weaken availability policy when recovery is impossible."

**End condition:** Baseline requirements met with three healthy workers. Distinguish actual service recovery from the hypothetical ability to survive another failure.

## Demo 6: direct SQL and CQ/SSE reconnection

**Start:** Reload baseline. Run `./demo sql` to open a SQL client connected to the **demo database only**.

1. Without touching the UI controls, execute:

   ```sql
   UPDATE gpu_telemetry AS t
   SET background_compute_units = 35
   FROM gpu_inventory AS g
   WHERE t.gpu_id = g.gpu_id
     AND g.host_id = 'inference-a'
     AND g.gpu_index = 0;
   ```

2. Show the configured value, generated demand, placement, and decision changing through the CQ/SSE pipeline. Database revision triggers apply to SQL edits as well as API edits.
3. Make only the browser offline using its developer tools. Leave Drasi, PostgreSQL, and the control service running. Wait for the UI to label retained data stale/disconnected.
4. Run the same SQL statement with **10** instead of 35. The server-side loop continues without a connected browser.
5. Bring the browser online. Wait for the package's supported query-snapshot/SSE resynchronization; use the appropriate scoped retry if it reports an error. Show the current saved value and placement.
6. Optionally open a second browser window: both views subscribe to CQs; neither contains the placement algorithm.

**Say:** "The browser is a projection of continuously maintained results. It is not the source of truth or the control loop."

**End condition:** All views are non-stale and agree on the current decision/version. Do not claim the reconnect is an exactly-once event replay; the package uses best-effort snapshots and visible recovery.

## Demo 7: regional recovery inside a data boundary

**Start:** Select **Load regional boundary**, or run:

```sh
./demo reset regional-boundary
./demo wait --timeout 120
```

1. Expand all three regional groups. Show eight ready replicas in West Europe, with North Europe and East US healthy and idle.
2. Select the Assistant workload. In **Processing policy**, show `customer-eu-processing`, its authority/revision, and the two allowed regions. North Europe is permitted; East US is prohibited for this workload, not unhealthy.
3. Click **Fail region** on `westeurope`. This powers off all six GPUs there and stops their reports. The settings acknowledgement is not itself a health event.
4. Wait for the five-second missing-report deadlines and the subsequent complete placement. All eight replicas must recover in North Europe, on its two workers/four GPUs. The 260-unit managed demand fits its 300-unit assignable budget, with service replicas on different workers.
5. Open the decision. Show the permitted-target evidence and the excluded East US region. No assignment may have used a US GPU, even transiently. A policy error or unknown result is not permission to fall back.
6. Fail `recovery-a`. After health expiry, the current result must be **infeasible within policy**: the surviving permitted worker has only two GPUs and 150 units, and cannot provide two worker separation domains.
7. Point to the four healthy US GPUs. Once the read-only capacity-only diagnostic finishes, show **Capacity available; no policy-compliant placement available**. It must not emit a candidate or cause a write. The still-authorized four replicas on `recovery-b` can continue; the system does not invent a partial success plan.
8. Select **Register ready worker**, name it `recovery-c`, choose cluster `eu-recovery` and profile `h100-nvl-pair-v1`. This adds already-ready capacity, not an instantaneous cloud provisioning operation.
9. Wait for fresh telemetry, a policy-current plan, application, and measurement: all eight replicas are ready again in North Europe. The fleet now has eight workers and sixteen registered GPUs, including failed devices. US GPUs remain unused.
10. Inspect resilience separately. Service has recovered, but it cannot recover from losing either currently healthy North Europe worker, or that region, while West Europe remains down. Do not require either resilience badge to be green.

**Say:** "The policy engine determines where this customer's data may be processed. The solver finds a placement inside that boundary. It does not override the boundary just because other GPUs are idle."

**End condition:** Eight replicas ready in permitted locations, current policy/placement evidence, no US assignments, and honestly degraded additional-failure resilience. Reload the fixture before demo 8; do not assume arbitrary equal-cost GPU choices.

## Demo 8: a running placement becomes prohibited

**Start:** Reload **regional-boundary**. Fail `westeurope` and wait for all eight replicas to recover in North Europe, as in steps 3-4 of demo 7.

1. Open **Processing policy** for `customer-eu-processing`. Show that the current running placements are allowed under the current revision.
2. Choose **Edit policy**. Remove `northeurope`, leaving `["westeurope"]`. Confirm the warning that this stricter policy can stop running workloads, then save with the current revision.
3. Show the database acknowledgement, followed by policy reevaluation. North Europe becomes prohibited despite its GPUs still being healthy. Policy changes, not missing heartbeats, drive this transition.
4. Wait for **fenced** acknowledgements for all eight replicas. Required count remains eight; running/ready counts become zero. While acknowledgements are pending, show **fencing-pending**, not a claim that processing has already stopped.
5. Show **no permitted healthy destination**: West Europe is still down, and North Europe/US are prohibited. The previous desired plan remains visible for explanation, but it must not keep executing. No empty success plan is written.
6. Click **Recover region** for `westeurope`. Wait for fresh reports, a newly validated plan, and eight ready replicas there under the stricter policy. Recovery of a device must not bypass current policy checks.
7. Restore the allowed-region list to `["westeurope","northeurope"]`. Wait for the new policy revision, any needed plan reauthorization, and current resilience results. Healthy work should stay in West Europe rather than move simply because more destinations became permitted.

**Say:** "A policy is not just an admission-time check. When the boundary changes, the system withdraws permission from existing placements and confirms that processing stopped. If no permitted alternative exists, it preserves the boundary rather than availability."

**End condition:** Eight ready replicas in West Europe, fresh policy and enforcement evidence, and both permitted regions available again. Show observation-to-fencing latency separately from CDC delay; do not claim instantaneous enforcement at the moment the policy was saved, or deletion of data previously accessed.

## Finish the presentation

Run `./demo down` to stop this demo while preserving its database. A later `./demo up` resumes that data; it does not silently load baseline. Use the preset loader explicitly before another presentation.

For a failed prerequisite, capture `./demo status` and the relevant service logs, then follow the [troubleshooting table](gpu-cluster-demo.md#132-troubleshooting). Do not advance with stale query data or manually seed a successful-looking placement.

## Presenter guardrails

- Wait on query-backed readiness, not a guessed delay; never advance while an explanation or resilience batch refers to an older signature.
- Show the solver's actual target choices and counts. Fixture scripts guarantee constraints and objective values, not arbitrary tie-breaking.
- Keep **demand units**, **busy percentage**, **memory reservation**, and **actual allocation** separately labeled.
- Label the compressed model-loading time, synthetic capacity profiles, and ready-worker registration honestly.
- Resilience checks must not change database placements. An explanatory or hypothetical result is not an executable plan.
- Keep fleet, regional cluster, and worker identities separate. One complete fleet plan governs all regions.
- A region denied to one workload is not globally down; all selected-workload highlights come from policy query results.
- Do not equate worker separation with regional availability or processing locality with complete legal/data-residency compliance.
- Missing policy evidence means unknown, not allow. Never relax policy automatically to make recovery succeed.
- Stop acknowledgement is separate from plan commitment. Ordinary capacity infeasibility can leave authorized work running; policy revocation cannot leave prohibited work running.
- If a prerequisite fails, show the error and reset through the fixture loader; do not patch the browser state to make the story continue.
