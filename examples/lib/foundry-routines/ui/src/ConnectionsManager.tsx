import { useState } from 'react';
import { Button, Dialog, DialogActions, DialogBody, DialogContent, DialogSurface, DialogTitle } from '@fluentui/react-components';
import { AddRegular, DismissRegular } from '@fluentui/react-icons';
import { useResource } from './hooks';
import ConnectionForm from './ConnectionForm';
import { Busy, ErrorNotice } from './shared';
import type { Connection, Resource, Schema } from './types';

export default function ConnectionsManager({ connections, revision, onSave, onClose }: {
  connections: Resource<Connection[]>;
  revision: number;
  onSave: (connection: Connection) => void;
  onClose: () => void;
}) {
  const [adding, setAdding] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const metadata = useResource<Schema>(selected ? `/api/connections/${encodeURIComponent(selected)}/schema` : null, revision);
  return <Dialog open onOpenChange={(_, d) => { if (!d.open) onClose(); }}>
    <DialogSurface className="connections-surface"><DialogBody>
      <DialogTitle action={<Button appearance="subtle" icon={<DismissRegular />} aria-label="Close source connections" onClick={onClose} />}>Source connections</DialogTitle>
      <DialogContent className="connections-content">
        <p className="muted">Reusable database connections for your routines. Saving configuration does not start replication or invoke an agent.</p>
        <ErrorNotice error={connections.error} />
        {connections.loading && !connections.data && <Busy label="Loading connections…" />}
        {!adding && <Button icon={<AddRegular />} onClick={() => setAdding(true)}>New connection</Button>}
        {adding && <ConnectionForm onCancel={() => setAdding(false)} onSave={connection => { onSave(connection); setAdding(false); }} />}
        {connections.data?.length === 0 && !adding && <div className="empty-state compact"><strong>No source connections</strong><p>Add PostgreSQL or MySQL, then reuse it in a routine's query setup.</p></div>}
        {connections.data?.map(connection => <div className="card" key={connection.id}>
          <div className="section-heading"><strong>{connection.name}</strong><Button size="small" onClick={() => setSelected(selected === connection.id ? null : connection.id)}>{selected === connection.id ? 'Hide schema' : 'Inspect schema'}</Button></div>
          <p className="muted small">{connection.kind} · {connection.host}:{connection.port} / {connection.database} · TLS {connection.tls}</p>
          <p className="schema-columns">Tables: {connection.tables.join(', ') || 'none'}<br />Key overrides: {connection.tableKeys.map(key => `${key.table} (${key.columns.join(', ')})`).join(' · ') || 'discovered primary keys'}</p>
          {selected === connection.id && <>
            <ErrorNotice error={metadata.error} />{metadata.loading && !metadata.data && <Busy label="Discovering schema…" />}
            {metadata.data && <>{metadata.data.checks.map((check, index) => <p key={index} className={check.ok ? 'muted' : 'warning-text'}>{check.ok ? '✓' : '!'} {check.name}: {check.detail}</p>)}
              {!metadata.data.checks.length && <p className="muted small">No readiness checks were returned.</p>}
              {metadata.data.tables.map(table => <p key={table.name} className="schema-columns"><strong>{table.label || table.name}</strong> · {table.columns.map(column => `${column.name}: ${column.type}${column.primaryKey ? ' [PK]' : ''}`).join(', ')}</p>)}</>}
          </>}
        </div>)}
        <p className="muted small">Connection settings and secret references persist across restart; secrets remain in the backend environment. This UI does not edit shared sources or change database privileges.</p>
      </DialogContent>
      <DialogActions><Button appearance="primary" onClick={onClose}>Done</Button></DialogActions>
    </DialogBody></DialogSurface>
  </Dialog>;
}
