import { useEffect, useRef, useState } from 'react';
import {
  Button, Checkbox, Dialog, DialogActions, DialogBody, DialogContent, DialogSurface, DialogTitle,
  Field, Input, MessageBar, MessageBarBody, Select, Tab, TabList, Textarea,
} from '@fluentui/react-components';
import { AddRegular, DismissRegular, PlayRegular } from '@fluentui/react-icons';
import { closePreview, errorMessage, request, startPreview, validateQuery } from './api';
import { routineProblems, serializeRoutine } from './config';
import { useResource } from './hooks';
import { Busy, CsvInput, ErrorNotice, JsonView } from './shared';
import ConnectionForm from './ConnectionForm';
import ResultsView from './ResultsView';
import type { Connection, Join, Routine, Schema, Snapshot } from './types';

export default function QuerySetup({ routine, connections, revision, onChange, onConnection, onClose }: {
  routine: Routine;
  connections: Connection[];
  revision: number;
  onChange: (routine: Routine) => void;
  onConnection: (connection: Connection) => void;
  onClose: () => void;
}) {
  const [step, setStep] = useState('sources');
  const [adding, setAdding] = useState(false);
  const [metadata, setMetadata] = useState<Record<string, Schema>>({});
  const [schemaErrors, setSchemaErrors] = useState<Record<string, Error>>({});
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const [validated, setValidated] = useState(false);
  const [previewId, setPreviewId] = useState<string | null>(null);
  const [previewRevision, setPreviewRevision] = useState(0);
  const [previewNote, setPreviewNote] = useState('');
  const preview = useResource<Snapshot>(previewId ? `/api/previews/${encodeURIComponent(previewId)}` : null, revision + previewRevision);
  const liveId = useRef<string | null>(null);
  const mounted = useRef(true);
  const sourceIds = routine.sources.map(source => source.connectionId).sort().join('|');
  const queryFingerprint = JSON.stringify([routine.query, routine.sources, routine.joins, routine.keyFields, routine.operations]);
  const sourceLabels = [...new Set(routine.sources.flatMap(source => source.labels))];

  useEffect(() => {
    const controller = new AbortController();
    for (const source of routine.sources) {
      void request<Schema>(`/api/connections/${encodeURIComponent(source.connectionId)}/schema`, 'GET', undefined, controller.signal).then(schema => {
        setMetadata(previous => ({ ...previous, [source.connectionId]: schema }));
        setSchemaErrors(previous => { const next = { ...previous }; delete next[source.connectionId]; return next; });
      }).catch(failure => {
        if (!controller.signal.aborted) setSchemaErrors(previous => ({ ...previous, [source.connectionId]: failure }));
      });
    }
    return () => controller.abort();
  }, [sourceIds, revision]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (liveId.current) void closePreview(liveId.current).catch(failure => {
        console.warn('Unable to release the isolated query preview:', errorMessage(failure));
      });
    };
  }, []);

  useEffect(() => {
    setValidated(false);
    if (liveId.current) {
      const old = liveId.current;
      liveId.current = null; setPreviewId(null);
      setPreviewNote('Preview stopped because the query configuration changed.');
      void closePreview(old).catch(setError);
    }
  }, [queryFingerprint]);

  useEffect(() => {
    if (!previewId) return;
    const poll = setInterval(() => setPreviewRevision(v => v + 1), 3000);
    const expiry = setTimeout(() => {
      liveId.current = null; setPreviewId(null);
      setPreviewNote('The two-minute preview window ended. Start another preview to continue.');
      void closePreview(previewId).catch(setError);
    }, 120_000);
    return () => { clearInterval(poll); clearTimeout(expiry); };
  }, [previewId]);

  async function stopPreview() {
    const id = liveId.current;
    if (!id) return;
    await closePreview(id);
    liveId.current = null; setPreviewId(null);
  }

  async function validate(previewRequested = false) {
    const candidate = serializeRoutine(routine);
    const problems = routineProblems(candidate, false);
    if (problems.length) { setError(new Error(problems.join(' '))); return; }
    setError(null); setBusy(true); setPreviewNote('');
    try {
      await stopPreview();
      if (previewRequested) {
        const result = await startPreview(candidate);
        if (!mounted.current) { await closePreview(result.id); return; }
        liveId.current = result.id; setPreviewId(result.id);
      } else {
        await validateQuery(candidate);
      }
      if (mounted.current) setValidated(true);
    } catch (failure) { if (mounted.current) setError(failure); }
    finally { if (mounted.current) setBusy(false); }
  }

  async function finish() {
    setBusy(true); setError(null);
    try { await stopPreview(); onClose(); }
    catch (failure) { setError(failure); }
    finally { if (mounted.current) setBusy(false); }
  }

  function columns(label: string) {
    const owner = routine.sources.find(source => source.labels.includes(label));
    const schema = owner ? metadata[owner.connectionId] : undefined;
    return schema?.tables.find(table => (table.label || table.name) === label)?.columns || [];
  }

  function updateJoin(index: number, key: keyof Join, value: string) {
    onChange({ ...routine, joins: routine.joins.map((join, i) => i === index ? { ...join, [key]: value } : join) });
  }

  return <Dialog open modalType="modal" onOpenChange={(_, data) => { if (!data.open && !busy) void finish(); }}>
    <DialogSurface className="query-surface">
      <DialogBody>
        <DialogTitle action={<Button className="dialog-dismiss" appearance="subtle" aria-label="Close query setup" icon={<DismissRegular />} disabled={busy} onClick={() => void finish()} />}>
          <span className="eyebrow">DRASI CONTINUOUS QUERY</span><br />Configure your trigger
        </DialogTitle>
        <p className="muted">Connect data, describe a condition, then choose which result changes should run your agent.</p>
        <TabList selectedValue={step} onTabSelect={(_, d) => setStep(String(d.value))} className="setup-tabs">
          <Tab value="sources">1 · Sources</Tab><Tab value="joins">2 · Relationships</Tab><Tab value="query">3 · Query & preview</Tab><Tab value="trigger">4 · Trigger policy</Tab>
        </TabList>
        <DialogContent className="setup-content">
          <ErrorNotice error={error} />
          {step === 'sources' && <>
            <div className="section-heading"><h3>Choose source connections</h3><Button icon={<AddRegular />} size="small" disabled={busy} onClick={() => setAdding(true)}>New connection</Button></div>
            <p className="muted">Each native table label belongs to one source. Subscriptions use labels from actual database metadata, not synthetic relationship names.</p>
            {adding && <ConnectionForm onCancel={() => setAdding(false)} onSave={connection => { onConnection(connection); setAdding(false); }} />}
            {!connections.length && !adding && <div className="empty-state compact"><strong>Start with a database connection</strong><p>Add PostgreSQL or MySQL, discover its tables, and select the labels for your query.</p></div>}
            {connections.map(connection => {
              const source = routine.sources.find(s => s.connectionId === connection.id);
              const schema = metadata[connection.id];
              return <div className="source-card card" key={connection.id}>
                <Checkbox disabled={busy} checked={!!source} label={`${connection.name} · ${connection.kind === 'postgres' ? 'PostgreSQL' : 'MySQL'}`} onChange={(_, d) => onChange({
                  ...routine, sources: d.checked
                    ? [...routine.sources, { connectionId: connection.id, labels: [...connection.tables] }]
                    : routine.sources.filter(s => s.connectionId !== connection.id),
                })} />
                <p className="muted small">{connection.host}:{connection.port} / {connection.database} · TLS {connection.tls}</p>
                {source && <>
                  <Field label="Allocated native labels (comma-separated)" hint={`Configured tables: ${connection.tables.join(', ') || 'none'}`}>
                    <CsvInput disabled={busy} value={source.labels} onChange={labels => onChange({
                      ...routine, sources: routine.sources.map(s => s.connectionId === connection.id ? { ...s, labels } : s),
                    })} />
                  </Field>
                  <ErrorNotice error={schemaErrors[connection.id]} />
                  {schema ? <>
                    <details><summary>Schema & readiness checks</summary>
                      {schema.checks.map((check, index) => <p key={index} className={check.ok ? 'muted' : 'warning-text'}>{check.ok ? '✓' : '!'} {check.name}: {check.detail}</p>)}
                      {!schema.checks.length && <p className="muted">No readiness checks reported.</p>}
                      {schema.tables.map(table => <p className="schema-columns" key={table.name}><strong>{table.label || table.name}</strong> · {table.columns.map(column => `${column.name} (${column.type}${column.primaryKey ? ', PK' : ''})`).join(', ')}</p>)}
                    </details>
                    {source.labels.some(label => !schema.tables.some(table => (table.label || table.name) === label)) && <p className="warning-text">An allocated label is not present in the discovered schema. Fix the selection before validation.</p>}
                  </> : !schemaErrors[connection.id] && <Busy label="Reading schema…" />}
                </>}
              </div>;
            })}
            <p className="muted small">Starting PostgreSQL may create its configured replication slot. Publications, permissions, and MySQL row-binlog settings are operator-managed; this UI does not change them.</p>
          </>}
          {step === 'joins' && <>
            <div className="section-heading"><h3>Equality relationships</h3><Button size="small" icon={<AddRegular />} disabled={busy} onClick={() => onChange({ ...routine, joins: [...routine.joins, { name: '', leftLabel: '', leftProperty: '', rightLabel: '', rightProperty: '' }] })}>Add relationship</Button></div>
            <p className="muted">Join properties with equal values across your native labels. Use the relationship name in Cypher. A single-source query may not need a join.</p>
            {!routine.joins.length && <div className="empty-state compact"><strong>No synthetic relationships</strong><p>Add one if your query connects data across labels.</p></div>}
            {routine.joins.map((join, index) => {
              const leftType = columns(join.leftLabel).find(column => column.name === join.leftProperty)?.type;
              const rightType = columns(join.rightLabel).find(column => column.name === join.rightProperty)?.type;
              return <div className="card" key={index}>
                <div className="section-heading"><h4>Relationship {index + 1}</h4><Button size="small" disabled={busy} onClick={() => onChange({ ...routine, joins: routine.joins.filter((_, i) => i !== index) })}>Remove</Button></div>
                <Field label="Relationship name"><Input disabled={busy} value={join.name} onChange={(_, d) => updateJoin(index, 'name', d.value)} /></Field>
                <div className="join-grid">{(['left', 'right'] as const).map(side => <div key={side}>
                  <Field label={`${side === 'left' ? 'Left' : 'Right'} native label`}><Select disabled={busy} value={join[`${side}Label`]} onChange={(_, d) => {
                    onChange({ ...routine, joins: routine.joins.map((value, i) => i === index ? { ...value, [`${side}Label`]: d.value, [`${side}Property`]: '' } : value) });
                  }}><option value="">Select a label</option>{sourceLabels.map(label => <option key={label}>{label}</option>)}</Select></Field>
                  <Field label="Equality property"><Input disabled={busy} list={`properties-${index}-${side}`} value={join[`${side}Property`]} onChange={(_, d) => updateJoin(index, `${side}Property`, d.value)} />
                    <datalist id={`properties-${index}-${side}`}>{columns(join[`${side}Label`]).map(column => <option key={column.name} value={column.name}>{column.type}</option>)}</datalist>
                  </Field>
                </div>)}</div>
                <p className="muted small">Property types: {leftType || 'unknown'} = {rightType || 'unknown'}. The backend validates compatibility; database type names can differ across connectors.</p>
              </div>;
            })}
          </>}
          {step === 'query' && <>
            <div className="section-heading"><h3>Describe the condition</h3><span className="code-language">Cypher</span></div>
            <p className="muted">Return only the fields the agent needs. ORDER BY, LIMIT, and SKIP are not supported for continuous result diffs.</p>
            <Field label="Continuous query"><Textarea className="code-editor" resize="vertical" rows={11} disabled={busy} spellCheck={false} placeholder="MATCH …&#10;WHERE …&#10;RETURN …" value={routine.query} onChange={(_, d) => onChange({ ...routine, query: d.value })} /></Field>
            <div className="schema-reference">Labels: {sourceLabels.join(', ') || 'select sources first'} · Relationships: {routine.joins.map(join => join.name).filter(Boolean).join(', ') || 'none'}</div>
            <Field label="Projected result-key fields" hint="Required for validation and preview. Use stable, non-null aliases from RETURN; combine fields with commas."><CsvInput disabled={busy} value={routine.keyFields} onChange={keyFields => onChange({ ...routine, keyFields })} /></Field>
            <div className="inline-actions">
              <Button disabled={busy} onClick={() => void validate()}>Validate query</Button>
              <Button appearance="primary" icon={<PlayRegular />} disabled={busy} onClick={() => void validate(true)}>Start live preview</Button>
              {previewId && <Button disabled={busy} onClick={() => void stopPreview().catch(setError)}>Stop preview</Button>}
              {busy && <Busy label="Validating query…" />}
            </div>
            <MessageBar intent="info"><MessageBarBody>Preview never invokes an agent or saves the routine. It starts an isolated query for up to two minutes; source startup may create the configured PostgreSQL replication slot.</MessageBarBody></MessageBar>
            {validated && <p className="valid-text">Query/configuration validation passed on the backend. Activation readiness is checked separately when starting.</p>}
            {previewNote && <p className="muted">{previewNote}</p>}
            <ErrorNotice error={preview.error} />
            {preview.loading && !preview.data && <Busy label="Waiting for preview snapshot…" />}
            {previewId && preview.data && <ResultsView snapshot={preview.data} preview />}
          </>}
          {step === 'trigger' && <>
            <h3>When should this agent run?</h3>
            <p className="muted">Each selected result change creates a fresh invocation. Changes to non-projected data that leave the result unchanged do not invoke an agent.</p>
            <div className="operation-cards">{(['added', 'updated', 'removed'] as const).map(operation => <div className="card" key={operation}>
              <Checkbox disabled={busy} checked={routine.operations.includes(operation)} label={`Result ${operation}`} onChange={(_, d) => onChange({ ...routine, operations: d.checked ? [...routine.operations, operation] : routine.operations.filter(op => op !== operation) })} />
              <p className="muted small">{operation === 'added' ? 'A condition enters the result set. Default.' : operation === 'updated' ? 'A projected result changes. Includes before & after.' : 'A condition leaves the result set. Does not cancel prior remote work.'}</p>
            </div>)}</div>
            <Field label="Result-key fields (comma-separated)" hint="Use stable, non-null aliases from RETURN. Composite keys are supported."><CsvInput disabled={busy} value={routine.keyFields} onChange={keyFields => onChange({ ...routine, keyFields })} /></Field>
            <div className="policy card"><h4>Future changes only</h4><p>Existing matches are visible after startup, but never invoke the agent during bootstrap. Pause keeps live results current and ignores the paused backlog. Resume starts from current state.</p></div>
            <h4>Agent input</h4><p className="muted">Your static prompt is attached to structured event data: routine revision, query epoch, sequence, operation, result key, before, after, and occurrence time. View the exact immutable payload in each run; preview data is never dispatched.</p>
            <h4>Static instructions</h4><pre className="query-code">{routine.prompt || 'Enter the static prompt in the routine dialog.'}</pre>
            <h4>Actual preview context</h4>
            {previewId && preview.data?.changes.length ? <><p className="muted small">A real event from this isolated preview, not an admitted run payload. Revision and activation metadata are attached only by the backend when dispatching.</p><JsonView value={preview.data.changes[preview.data.changes.length - 1]} label="Actual preview event context" /></>
              : <p className="muted small">Start a live preview in step 3 and change source data to inspect actual before/after context here. No sample event or run is invented.</p>}
          </>}
        </DialogContent>
        <DialogActions className="setup-footer">
          <span className="muted small">Query configuration is not saved until you save the routine.</span>
          {step !== 'sources' && <Button disabled={busy} onClick={() => setStep(['sources', 'joins', 'query', 'trigger'][['sources', 'joins', 'query', 'trigger'].indexOf(step) - 1])}>Back</Button>}
          {step !== 'trigger' ? <Button appearance="primary" disabled={busy} onClick={() => setStep(['sources', 'joins', 'query', 'trigger'][['sources', 'joins', 'query', 'trigger'].indexOf(step) + 1])}>Continue</Button> : <Button appearance="primary" disabled={busy} onClick={() => void finish()}>Done</Button>}
        </DialogActions>
      </DialogBody>
    </DialogSurface>
  </Dialog>;
}
