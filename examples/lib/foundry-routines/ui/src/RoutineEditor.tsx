import { useState } from 'react';
import {
  Button, Dialog, DialogActions, DialogBody, DialogContent, DialogSurface, DialogTitle,
  Field, Input, MessageBar, MessageBarBody, Select, Textarea,
} from '@fluentui/react-components';
import { DismissRegular, FlowRegular, LockClosedRegular } from '@fluentui/react-icons';
import { request, routinePath, saveDraft } from './api';
import { newRoutine, routineProblems, serializeRoutine } from './config';
import { Busy, ErrorNotice } from './shared';
import QuerySetup from './QuerySetup';
import type { Agent, Connection, FoundryStatus, Resource, Routine } from './types';

export default function RoutineEditor({ initial, agents, foundry, connections, revision, onConnection, onSaved, onClose }: {
  initial?: Routine;
  agents: Resource<Agent[]>;
  foundry: Resource<FoundryStatus>;
  connections: Resource<Connection[]>;
  revision: number;
  onConnection: (connection: Connection) => void;
  onSaved: (routine: Routine) => void;
  onClose: () => void;
}) {
  const [routine, setRoutine] = useState<Routine>(() => initial ? structuredClone(initial) : newRoutine());
  const [setup, setSetup] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [savedNotice, setSavedNotice] = useState('');
  const isEditing = !!initial?.id;

  async function save(activate: boolean) {
    const candidate = serializeRoutine(routine);
    const problems = activate ? routineProblems(candidate) : !candidate.name ? ['Enter a routine name.'] : [];
    if (problems.length) { setError(new Error(problems.join(' '))); return; }
    setError(null); setBusy(true); setSavedNotice('');
    try {
      const draft = await saveDraft(candidate);
      setRoutine(draft);
      if (activate) {
        setSavedNotice('Draft saved. Establishing the activation boundary…');
        await request(`${routinePath(draft.id)}/activate`, 'POST');
      }
      onSaved(draft);
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  return <>
    <Dialog open={!setup} onOpenChange={(_, data) => { if (!data.open && !busy) onClose(); }}>
      <DialogSurface className="routine-surface">
        <DialogBody>
          <DialogTitle action={<Button className="dialog-dismiss" appearance="subtle" aria-label="Close routine editor" disabled={busy} icon={<DismissRegular />} onClick={onClose} />}>{isEditing ? 'Edit routine' : 'New routine'}</DialogTitle>
          <DialogContent className="routine-editor">
            <p className="muted">Run an existing agent when a continuous query result changes.</p>
            <ErrorNotice error={error} />
            {savedNotice && <MessageBar intent="info"><MessageBarBody>{savedNotice} If starting fails, this routine remains saved; inspect the error before retrying.</MessageBarBody></MessageBar>}
            <Field label="Name" required><Input disabled={busy} autoFocus value={routine.name} placeholder="Give your routine a clear purpose" onChange={(_, d) => setRoutine({ ...routine, name: d.value })} /></Field>
            <Field label="Agent" required hint="Choose a discovered agent or type an existing agent name. This does not create an agent.">
              <Input disabled={busy} list="existing-agent-names" value={routine.agent} placeholder={agents.loading ? 'Discovering agents…' : 'Existing agent name'} onChange={(_, d) => setRoutine({ ...routine, agent: d.value })} />
              <datalist id="existing-agent-names">{agents.data?.map(agent => <option key={agent.name} value={agent.name}>{agent.kind}{agent.offline ? ' · offline test' : ''}</option>)}</datalist>
            </Field>
            <ErrorNotice error={agents.error} />
            {!agents.loading && !agents.error && agents.data?.length === 0 && <p className="muted small">No compatible agents returned. A manually configured name will be checked by the backend.</p>}
            <Field label="Authorization">
              <div className="identity-row"><LockClosedRegular /><div><strong>{foundry.data?.identity || 'Backend identity not available'}</strong><p className="muted small">Actual backend developer identity · native agent/creator identity choices are not implemented.</p></div></div>
            </Field>
            <ErrorNotice error={foundry.error} />
            {foundry.data && !foundry.data.configured && <p className="warning-text small">Foundry is not configured. Drafts and query-only previews are still available; starting a live routine requires backend configuration.</p>}
            <Field label="Prompt" required hint="Static instructions only. The backend attaches structured event data; there is no template interpolation.">
              <Textarea disabled={busy} rows={3} resize="vertical" value={routine.prompt} placeholder="Explain the changed condition and recommend a response." onChange={(_, d) => setRoutine({ ...routine, prompt: d.value })} />
            </Field>
            <Field label="Trigger"><Select disabled={busy} value="continuous">
              <option value="continuous">Continuous Query · powered by Drasi</option>
              <option disabled>Schedule · contextual, not implemented</option>
              <option disabled>GitHub issue · contextual, not implemented</option>
              <option disabled>Teams message · contextual, not implemented</option>
            </Select></Field>
            <div className="trigger-summary">
              <FlowRegular /><div><strong>Continuous Query</strong><p className="muted small">{routine.sources.length ? `${routine.sources.length} source connection${routine.sources.length === 1 ? '' : 's'} · ${routine.joins.length} equality relationship${routine.joins.length === 1 ? '' : 's'}` : 'Connect your databases and define a condition.'}</p><p className="muted small">Operations: {routine.operations.join(', ') || 'none'} · key: {routine.keyFields.join(', ') || 'not configured'}</p></div>
              <Button size="small" disabled={busy} onClick={() => setSetup(true)}>Configure query</Button>
            </div>
            <ErrorNotice error={connections.error} />
            <p className="muted small">First activation invokes only future changes, not existing matches. Paused changes are never replayed. Saving a draft never invokes an agent.</p>
          </DialogContent>
          <DialogActions>
            <Button disabled={busy} onClick={onClose}>Cancel</Button>
            <Button disabled={busy} onClick={() => void save(false)}>Save draft</Button>
            <Button appearance="primary" disabled={busy} onClick={() => void save(true)}>{busy ? 'Saving…' : isEditing ? 'Save & start' : 'Create & start'}</Button>
          </DialogActions>
          {busy && <Busy label="Saving routine…" />}
        </DialogBody>
      </DialogSurface>
    </Dialog>
    {setup && <QuerySetup routine={routine} connections={connections.data || []} revision={revision} onChange={setRoutine} onConnection={onConnection} onClose={() => setSetup(false)} />}
  </>;
}
