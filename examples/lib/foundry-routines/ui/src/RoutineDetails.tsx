import { useState } from 'react';
import {
  Button, Dialog, DialogActions, DialogBody, DialogContent, DialogSurface, DialogTitle,
  Field, Tab, TabList, Table, TableBody, TableCell, TableHeader, TableHeaderCell, TableRow,
  OverlayDrawer, DrawerHeader, DrawerHeaderTitle, DrawerBody,
} from '@fluentui/react-components';
import { ArrowLeftRegular, DeleteRegular, DismissRegular, EditRegular, PauseRegular, PlayRegular } from '@fluentui/react-icons';
import { request, routinePath } from './api';
import { useResource } from './hooks';
import { asObject, Busy, cell, ErrorNotice, JsonView, StatusBadge, timestamp } from './shared';
import ResultsView from './ResultsView';
import type { Connection, FoundryStatus, Routine, Run, Snapshot } from './types';

export default function RoutineDetails({ id, revision, connections, foundry, onBack, onEdit, onRefresh, onDeleted }: {
  id: string;
  revision: number;
  connections: Connection[];
  foundry: FoundryStatus | null;
  onBack: () => void;
  onEdit: (routine: Routine) => void;
  onRefresh: () => void;
  onDeleted: () => void;
}) {
  const [tab, setTab] = useState('configuration');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [confirm, setConfirm] = useState(false);
  const [runId, setRunId] = useState<string | null>(null);
  const routine = useResource<Routine>(routinePath(id), revision);
  const results = useResource<Snapshot>(tab === 'results' ? `${routinePath(id)}/results` : null, revision);
  const runs = useResource<Run[]>(tab === 'runs' ? `${routinePath(id)}/runs?limit=50` : null, revision);
  const run = useResource<Run>(runId ? `/api/runs/${encodeURIComponent(runId)}` : null, revision);

  async function action(name: 'activate' | 'pause' | 'resume' | 'delete') {
    setBusy(true); setError(null);
    try {
      await request(name === 'delete' ? routinePath(id) : `${routinePath(id)}/${name}`, name === 'delete' ? 'DELETE' : 'POST');
      if (name === 'delete') onDeleted(); else onRefresh();
      setConfirm(false);
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  const value = routine.data;
  return <section className="routine-details">
    <Button appearance="subtle" icon={<ArrowLeftRegular />} onClick={onBack}>All routines</Button>
    <ErrorNotice error={routine.error} />
    <ErrorNotice error={error} />
    {!value && routine.loading && <Busy label="Loading routine…" />}
    {value && <>
      <div className="detail-heading">
        <div><div className="eyebrow">CONTINUOUS QUERY ROUTINE</div><h1>{value.name}</h1><div className="inline-actions"><StatusBadge status={value.status} /><span className="muted small">Revision {value.revision} · {value.agent || 'No agent configured'}</span></div></div>
        <div className="inline-actions">
          <Button icon={<EditRegular />} disabled={busy || ['Active', 'Starting'].includes(value.status)} title={['Active', 'Starting'].includes(value.status) ? 'Pause the routine before editing its immutable revision.' : 'Edit configuration'} onClick={() => onEdit(value)}>Edit</Button>
          {value.status === 'Active' ? <Button icon={<PauseRegular />} disabled={busy} onClick={() => void action('pause')}>Pause</Button>
            : value.status === 'Starting' ? <Button disabled>Starting…</Button>
            : <Button appearance="primary" icon={<PlayRegular />} disabled={busy} onClick={() => void action(value.status === 'Draft' ? 'activate' : 'resume')}>{value.status === 'Draft' ? 'Start' : 'Resume'}</Button>}
          <Button icon={<DeleteRegular />} aria-label="Delete routine" disabled={busy} onClick={() => setConfirm(true)} />
        </div>
      </div>
      {value.error && <ErrorNotice error={new Error(value.error)} />}
      {value.status === 'Interrupted' && <div className="notice warning-text">This routine was interrupted. Configuration and run history persist, but in-memory query state must be rebuilt. Resume ignores offline and paused backlog.</div>}
      <TabList selectedValue={tab} onTabSelect={(_, d) => setTab(String(d.value))}><Tab value="configuration">Configuration</Tab><Tab value="results">Live results</Tab><Tab value="runs">Runs</Tab></TabList>
      <div className="detail-content">
        {tab === 'configuration' && <div className="configuration-grid">
          <section className="card"><h3>Source connections</h3>
            {!value.sources.length && <p className="muted">No source connections configured.</p>}
            {value.sources.map(source => {
              const connection = connections.find(c => c.id === source.connectionId);
              return <div className="config-source" key={source.connectionId}><strong>{connection?.name || source.connectionId}</strong><p className="muted small">{connection ? `${connection.kind} · ${connection.host}:${connection.port} / ${connection.database}` : 'Connection details unavailable'}</p><p>Labels: <code>{source.labels.join(', ') || 'none'}</code></p></div>;
            })}
            <h4>Equality relationships</h4>{value.joins.length ? value.joins.map((join, i) => <p className="schema-columns" key={i}><strong>{join.name}</strong><br />{join.leftLabel}.{join.leftProperty} = {join.rightLabel}.{join.rightProperty}</p>) : <p className="muted">No synthetic relationships configured.</p>}
          </section>
          <section className="card"><h3>Agent & authorization</h3><Field label="Existing agent"><strong>{value.agent || 'Not configured'}</strong></Field><Field label="Actual backend identity"><span>{foundry?.identity || 'Identity unavailable'}</span></Field><p className="muted small">Integration concept, not a native Foundry routine. Agent invocation and run history are managed by this local backend.</p><h4>Static prompt</h4><p className="prompt-text">{value.prompt || 'Not configured'}</p></section>
          <section className="card full-width"><div className="section-heading"><h3>Continuous query</h3><span className="code-language">Cypher</span></div><pre className="query-code">{value.query || 'No query configured'}</pre>
            <div className="policy-columns"><div><h4>Result operations</h4><p>{value.operations.join(', ') || 'None selected'}</p></div><div><h4>Stable result key</h4><p>{value.keyFields.join(', ') || 'Not configured'}</p></div><div><h4>Startup & pause</h4><p className="muted small">Future changes only. No bootstrap invocation or paused backlog burst. Already submitted remote work may finish after Pause.</p></div></div>
          </section>
        </div>}
        {tab === 'results' && <><ErrorNotice error={results.error} />{results.loading && !results.data && <Busy label="Loading authoritative results…" />}{results.data && <ResultsView snapshot={results.data} />}</>}
        {tab === 'runs' && <>
          <div className="section-heading"><h3>Agent runs</h3><span className="muted small">Most recent 50 · local durable history</span></div>
          <p className="muted small">Unknown means remote acceptance could not be determined; it is not silently retried. No native Foundry run count or trace link is fabricated.</p>
          <ErrorNotice error={runs.error} />{runs.loading && !runs.data && <Busy label="Loading run history…" />}
          {runs.data?.length === 0 && <div className="empty-state compact"><strong>No runs yet</strong><p>Preview and bootstrap never invoke agents. Runs appear after a selected future result change.</p></div>}
          {!!runs.data?.length && <div className="table-scroll"><Table size="small" aria-label="Agent run history">
            <TableHeader><TableRow><TableHeaderCell>Run</TableHeaderCell><TableHeaderCell>Status</TableHeaderCell><TableHeaderCell>Operation / key</TableHeaderCell><TableHeaderCell>Created</TableHeaderCell><TableHeaderCell>Updated</TableHeaderCell></TableRow></TableHeader>
            <TableBody>{runs.data.map(record => {
              const event = asObject(asObject(record.input).event);
              return <TableRow key={record.id}><TableCell><Button appearance="transparent" size="small" onClick={() => setRunId(record.id)}>{record.id}</Button></TableCell><TableCell><StatusBadge status={record.status} /></TableCell><TableCell>{cell(event.operation)} / {cell(event.resultKey)}</TableCell><TableCell>{timestamp(record.createdAt)}</TableCell><TableCell>{timestamp(record.updatedAt)}</TableCell></TableRow>;
            })}</TableBody>
          </Table></div>}
        </>}
      </div>
    </>}
    <Dialog open={confirm} onOpenChange={(_, d) => { if (!busy) setConfirm(d.open); }}>
      <DialogSurface><DialogBody><DialogTitle>Delete “{value?.name || id}”?</DialogTitle><DialogContent>This removes the routine and its query/reaction, not shared source connections or database resources. Already accepted remote work is not implicitly cancelled.<ErrorNotice error={error} /></DialogContent><DialogActions><Button disabled={busy} onClick={() => setConfirm(false)}>Cancel</Button><Button appearance="primary" disabled={busy} onClick={() => void action('delete')}>Delete routine</Button></DialogActions></DialogBody></DialogSurface>
    </Dialog>
    <OverlayDrawer open={!!runId} position="end" size="large" onOpenChange={(_, d) => { if (!d.open) setRunId(null); }}>
      <DrawerHeader><DrawerHeaderTitle action={<Button appearance="subtle" icon={<DismissRegular />} aria-label="Close run details" onClick={() => setRunId(null)} />}>Run details</DrawerHeaderTitle></DrawerHeader>
      <DrawerBody>
        <ErrorNotice error={run.error} />{run.loading && !run.data && <Busy label="Loading run…" />}
        {run.data && <><StatusBadge status={run.data.status} /><h3 className="break-anywhere">{run.data.id}</h3><p className="muted">Agent: {run.data.agent}</p><p className="muted small">Created {timestamp(run.data.createdAt)}<br />Updated {timestamp(run.data.updatedAt)}</p>
          <ErrorNotice error={run.data.error ? new Error(run.data.error) : null} />
          <h3>Immutable input</h3><p className="muted small">The exact stored prompt and structured context attached to this run, including its original revision and event sequence.</p><JsonView value={run.data.input} label="Immutable agent input" />
          <h3>Returned response / output</h3>{run.data.response === null ? <p className="muted">No response recorded. Inspect status and errors; this is not a completed response.</p> : <JsonView value={run.data.response} label="Actual agent response" />}
        </>}
      </DrawerBody>
    </OverlayDrawer>
  </section>;
}
