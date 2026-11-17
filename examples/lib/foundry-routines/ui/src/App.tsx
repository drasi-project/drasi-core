import { useState } from 'react';
import {
  Badge, Button, Input, Select, Tab, TabList, Table, TableBody, TableCell, TableHeader,
  TableHeaderCell, TableRow,
} from '@fluentui/react-components';
import {
  AddRegular, ArrowClockwiseRegular, BeakerRegular, BotRegular, ChevronRightRegular,
  CubeRegular, DatabaseRegular, FlowRegular, HomeRegular, SearchRegular, SettingsRegular,
} from '@fluentui/react-icons';
import { routinePath } from './api';
import { useLiveRefresh, useResource } from './hooks';
import { Busy, ErrorNotice, StatusBadge, timestamp, TransportBadge } from './shared';
import RoutineEditor from './RoutineEditor';
import RoutineDetails from './RoutineDetails';
import DemoDrawer from './DemoDrawer';
import ConnectionsManager from './ConnectionsManager';
import type { Agent, Connection, FoundryStatus, Health, Routine, Run } from './types';

function RoutineRow({ routine, revision, onOpen }: { routine: Routine; revision: number; onOpen: () => void }) {
  const lastRun = useResource<Run[]>(`${routinePath(routine.id)}/runs?limit=1`, revision);
  return <TableRow>
    <TableCell><Button appearance="transparent" className="routine-name" icon={<FlowRegular />} onClick={onOpen}>{routine.name}</Button></TableCell>
    <TableCell><StatusBadge status={routine.status} />{routine.error && <span className="row-error" title={routine.error}>Error details in routine</span>}</TableCell>
    <TableCell><span className="agent-cell"><BotRegular />{routine.agent || 'Not configured'}</span></TableCell>
    <TableCell><span className="trigger-cell"><span className="purple-dot" />Continuous Query</span></TableCell>
    <TableCell>{lastRun.error ? <span className="warning-text" title={lastRun.error.message}>Unavailable</span> : !lastRun.data ? 'Loading…' : lastRun.data.length ? timestamp(lastRun.data[0].createdAt) : 'No runs yet'}</TableCell>
  </TableRow>;
}

export default function App() {
  const { revision, stream, refresh } = useLiveRefresh();
  const health = useResource<Health>('/api/health', revision);
  const foundry = useResource<FoundryStatus>('/api/foundry/status', revision);
  const agents = useResource<Agent[]>('/api/agents', revision);
  const connections = useResource<Connection[]>('/api/connections', revision);
  const routines = useResource<Routine[]>('/api/routines', revision);
  const [tab, setTab] = useState('routines');
  const [selected, setSelected] = useState<string | null>(null);
  const [editor, setEditor] = useState<{ initial?: Routine } | null>(null);
  const [demo, setDemo] = useState(false);
  const [connectionsOpen, setConnectionsOpen] = useState(false);
  const [search, setSearch] = useState('');
  const [status, setStatus] = useState('all');
  const [savedConnections, setSavedConnections] = useState<Connection[]>([]);
  const allConnections = [...(connections.data || [])];
  for (const connection of savedConnections) if (!allConnections.some(c => c.id === connection.id)) allConnections.push(connection);
  const filtered = routines.data?.filter(routine =>
    (status === 'all' || routine.status === status)
    && `${routine.name} ${routine.agent}`.toLowerCase().includes(search.toLowerCase()),
  );
  const mode = health.data?.mode || foundry.data?.mode;
  const offline = health.data?.mode === 'offline' || foundry.data?.mode === 'offline';

  return <div className="app-shell">
    <header className="global-header">
      <div className="foundry-brand"><div className="brand-mark"><CubeRegular /></div><strong>Microsoft Foundry</strong><span className="brand-divider" /><span className="muted">Drasi integration concept</span></div>
      <div className="header-status"><TransportBadge mode={offline ? 'offline' : mode} /><span className="identity-pill" title={foundry.data?.identity || 'Backend identity unavailable'}><span className="identity-dot" />{foundry.data?.identity || 'Local backend'}</span></div>
    </header>
    <aside className="sidebar">
      <div className="project-switch"><span className="project-icon"><CubeRegular /></span><div><strong>Drasi integration concept</strong><p>Local project experience</p></div></div>
      <div className="nav-heading">PROJECT</div>
      <Button appearance="subtle" icon={<HomeRegular />} disabled>Overview</Button>
      <div className="nav-heading">BUILD</div>
      <Button appearance="subtle" icon={<BotRegular />} className={tab === 'agents' ? 'nav-active' : ''} onClick={() => { setTab('agents'); setSelected(null); }}>Agents</Button>
      <Button appearance="subtle" icon={<FlowRegular />} className={tab === 'routines' ? 'nav-active' : ''} onClick={() => { setTab('routines'); setSelected(null); }}>Routines <Badge size="small" appearance="tint" color="brand">Concept</Badge></Button>
      <Button appearance="subtle" icon={<CubeRegular />} disabled title="Workflow agents are outside this integration concept.">Workflows</Button>
      <div className="nav-heading">MANAGE</div>
      <Button appearance="subtle" icon={<DatabaseRegular />} onClick={() => setConnectionsOpen(true)}>Source connections</Button>
      <Button appearance="subtle" icon={<SettingsRegular />} disabled>Project settings</Button>
      <div className="sidebar-footer"><div className="powered-mark"><FlowRegular /><strong>Powered by Drasi</strong></div><p>Reacting to changing conditions,<br />not another scheduled check.</p><span className={`stream-dot ${stream}`} />{stream === 'connected' ? 'Live refresh connected' : stream === 'connecting' ? 'Connecting live refresh…' : 'Refresh disconnected · retrying'}</div>
    </aside>
    <main className="main-content">
      <div className="breadcrumb"><span>Microsoft Foundry</span><ChevronRightRegular /><span>Drasi integration concept</span></div>
      <div className="project-heading"><div><h2>Build</h2><p className="muted">Turn your agents into always-aware assistants.</p></div><span className="concept-label">Integration concept · powered by Drasi</span></div>
      <TabList selectedValue={tab} onTabSelect={(_, d) => { setTab(String(d.value)); setSelected(null); }}><Tab value="agents">Agents</Tab><Tab value="routines">Routines</Tab><Tab value="workflows" disabled>Workflows</Tab></TabList>
      <div className="workspace">
        <ErrorNotice error={health.error} /><ErrorNotice error={foundry.error} />
        {health.data && !health.data.ready && <div className="notice warning-text">The backend is not ready. Check backend startup diagnostics before starting a routine.</div>}
        {offline && <div className="offline-notice"><BeakerRegular /><span><strong>OFFLINE TEST TRANSPORT</strong> — responses use the explicit test transport, not a live Foundry agent. Database query activity is separate.</span></div>}
        {!offline && foundry.data && !foundry.data.configured && <div role="status" className="notice warning-text"><strong>Configure live Foundry</strong><p>Set <code>FOUNDRY_PROJECT_ENDPOINT</code> in <code>examples/lib/foundry-routines/.env</code>, then restart the backend. Authentication uses your existing default Azure developer credentials; no API key or token is needed.</p></div>}
        {stream === 'disconnected' && <div role="status" className="notice warning-text">Live refresh is disconnected. Reconnecting automatically; authoritative data is refreshed every 15 seconds while disconnected. <Button size="small" onClick={refresh}>Refresh now</Button></div>}
        {tab === 'agents' ? <section>
          <div className="page-heading"><div><h1>Agents</h1><p className="muted">Compatible existing agents discovered by the backend.</p></div><Button icon={<ArrowClockwiseRegular />} onClick={refresh}>Refresh</Button></div>
          <ErrorNotice error={agents.error} />{agents.loading && !agents.data && <Busy label="Discovering compatible agents…" />}
          <p className="muted small">Project: {foundry.data?.project || 'not configured'} · Identity: {foundry.data?.identity || 'unavailable'}</p>
          {agents.data?.length === 0 && <div className="empty-state"><BotRegular /><h3>No compatible agents returned</h3><p>Create or configure an agent in Foundry separately. You can enter an existing agent name manually when creating a routine.</p></div>}
          <div className="agent-grid">{agents.data?.map(agent => <article className="card" key={agent.name}><BotRegular /><h3>{agent.name}</h3><p className="muted">{agent.kind}</p>{agent.offline && <TransportBadge mode="offline" />}<Button size="small" onClick={() => { setTab('routines'); setEditor({ initial: { id: '', name: '', agent: agent.name, prompt: '', query: '', sources: [], joins: [], keyFields: [], operations: ['added'], revision: 0, status: 'Draft', error: null } }); }}>Use in a routine</Button></article>)}</div>
        </section> : selected ? <RoutineDetails id={selected} revision={revision} connections={allConnections} foundry={foundry.data} onBack={() => setSelected(null)} onEdit={initial => setEditor({ initial })} onRefresh={refresh} onDeleted={() => { setSelected(null); refresh(); }} /> : <section>
          <div className="page-heading"><div><h1>Routines</h1><p className="muted">Run an agent when your data meets a changing condition.</p></div><Button appearance="primary" icon={<AddRegular />} onClick={() => setEditor({})}>New routine</Button></div>
          <div className="intro-strip"><span className="intro-icon"><FlowRegular /></span><div><strong>From data changes to meaningful action</strong><p>Connect sources → define a continuous query → select result changes → run your agent.</p></div><span className="intro-tag">Continuous Query</span></div>
          <div className="table-toolbar"><div className="table-filters"><Input contentBefore={<SearchRegular />} placeholder="Search routines or agents" aria-label="Search routines" value={search} onChange={(_, d) => setSearch(d.value)} /><Select aria-label="Filter routine status" value={status} onChange={(_, d) => setStatus(d.value)}><option value="all">All statuses</option>{['Draft', 'Starting', 'Active', 'Paused', 'Error', 'Interrupted'].map(value => <option key={value}>{value}</option>)}</Select></div><div className="inline-actions">{health.data?.fixtures && <Button icon={<BeakerRegular />} onClick={() => setDemo(true)}>Demo data</Button>}<Button appearance="subtle" icon={<ArrowClockwiseRegular />} aria-label="Refresh routines" onClick={refresh} /></div></div>
          <ErrorNotice error={routines.error} />
          {routines.loading && !routines.data && <Busy label="Loading routines…" />}
          {routines.data && <div className="routine-table table-scroll"><Table size="small" aria-label="Routines">
            <TableHeader><TableRow><TableHeaderCell>Routine</TableHeaderCell><TableHeaderCell>Status</TableHeaderCell><TableHeaderCell>Agent</TableHeaderCell><TableHeaderCell>Trigger</TableHeaderCell><TableHeaderCell>Last run</TableHeaderCell></TableRow></TableHeader>
            <TableBody>{filtered?.map(routine => <RoutineRow key={routine.id} routine={routine} revision={revision} onOpen={() => setSelected(routine.id)} />)}</TableBody>
          </Table></div>}
          {filtered?.length === 0 && <div className="empty-state"><div className="empty-icon"><FlowRegular /></div><h3>{routines.data?.length ? 'No routines match your filters' : 'Your first always-aware routine starts here'}</h3><p>{routines.data?.length ? 'Try another search or status.' : 'Choose an existing agent, connect your sources, and preview a condition before starting. No agent calls happen until you activate.'}</p>{!routines.data?.length && <Button appearance="primary" icon={<AddRegular />} onClick={() => setEditor({})}>Create a routine</Button>}</div>}
          <p className="table-footnote">Local routine definitions and durable run history · future changes only · no native Foundry routine is created</p>
        </section>}
      </div>
    </main>
    {editor && <RoutineEditor initial={editor.initial} agents={agents} foundry={foundry} connections={{ ...connections, data: allConnections }} revision={revision}
      onConnection={connection => { setSavedConnections(previous => [...previous.filter(c => c.id !== connection.id), connection]); refresh(); }}
      onSaved={routine => { setEditor(null); setSelected(routine.id); refresh(); }} onClose={() => setEditor(null)} />}
    {demo && health.data?.fixtures && <DemoDrawer revision={revision} onClose={() => setDemo(false)} onRefresh={refresh} />}
    {connectionsOpen && <ConnectionsManager connections={{ ...connections, data: allConnections }} revision={revision}
      onSave={connection => { setSavedConnections(previous => [...previous.filter(c => c.id !== connection.id), connection]); refresh(); }}
      onClose={() => setConnectionsOpen(false)} />}
  </div>;
}
