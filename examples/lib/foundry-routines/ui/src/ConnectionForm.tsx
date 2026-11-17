import { useState } from 'react';
import { Button, Checkbox, Field, Input, MessageBar, MessageBarBody, Select } from '@fluentui/react-components';
import { request } from './api';
import { connectionProblems, newConnection, serializeConnection } from './config';
import { Busy, CsvInput, ErrorNotice } from './shared';
import type { Connection, Schema } from './types';

export default function ConnectionForm({ onSave, onCancel }: {
  onSave: (connection: Connection) => void;
  onCancel: () => void;
}) {
  const [connection, setConnection] = useState(newConnection);
  const [schema, setSchema] = useState<Schema | null>(null);
  const [validated, setValidated] = useState('');
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState(false);
  const payload = serializeConnection(connection);
  const problems = connectionProblems(payload, schema);
  const isCurrent = validated === JSON.stringify(payload);

  function update<K extends keyof Connection>(key: K, value: Connection[K]) {
    setConnection(previous => ({ ...previous, [key]: value }));
    setError(null);
    if (!['tables', 'tableKeys'].includes(key)) { setSchema(null); setValidated(''); }
  }

  async function validate() {
    if (connectionProblems(payload).length) {
      setError(new Error(connectionProblems(payload).join(' '))); return;
    }
    setBusy(true); setError(null);
    try {
      const result = await request<Schema>('/api/connections/validate', 'POST', payload);
      setSchema(result); setValidated(JSON.stringify(payload));
    } catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  async function save() {
    setBusy(true); setError(null);
    try { onSave(await request<Connection>('/api/connections', 'POST', payload)); }
    catch (failure) { setError(failure); }
    finally { setBusy(false); }
  }

  return <section className="connection-form card">
    <div className="section-heading"><h3>New source connection</h3><Button size="small" disabled={busy} onClick={onCancel}>Cancel</Button></div>
    <p className="muted">Read-only discovery checks connectivity and CDC prerequisites. It does not start replication or change database settings.</p>
    <fieldset disabled={busy} className="form-grid">
      <Field label="Connection name" required><Input value={connection.name} onChange={(_, d) => update('name', d.value)} /></Field>
      <Field label="Database connector"><Select value={connection.kind} onChange={(_, d) => {
        const kind = d.value as Connection['kind'];
        setConnection(previous => ({ ...previous, kind, port: kind === 'postgres' ? 5432 : 3306, tables: [], tableKeys: [] }));
        setSchema(null); setValidated('');
      }}><option value="postgres">PostgreSQL</option><option value="mysql">MySQL</option></Select></Field>
      <Field label="Host" required><Input value={connection.host} onChange={(_, d) => update('host', d.value)} /></Field>
      <Field label="Port" required><Input type="number" min={1} max={65535} value={String(connection.port)} onChange={(_, d) => update('port', Number(d.value))} /></Field>
      <Field label="Database" required><Input value={connection.database} onChange={(_, d) => update('database', d.value)} /></Field>
      <Field label="Database user" required><Input value={connection.user} onChange={(_, d) => update('user', d.value)} autoComplete="off" /></Field>
      <Field label="Password environment reference" required hint="Set this alias on the backend. Never enter a literal password."><Input
        placeholder="FOUNDRY_DEMO_…"
        value={connection.passwordRef}
        onChange={(_, d) => update('passwordRef', d.value)}
        autoComplete="off"
      /></Field>
      <Field label="TLS"><Select value={connection.tls} onChange={(_, d) => update('tls', d.value as Connection['tls'])}>
        <option value="verify">Verify certificate</option><option value="disabled">Disabled · local fixtures only</option>
      </Select></Field>
      {connection.kind === 'postgres' ? <>
        <Field label="Publication" hint="Existing publication configured by the operator."><Input value={connection.publication} onChange={(_, d) => update('publication', d.value)} /></Field>
        <Field label="Replication slot" hint="Starting this source may create the configured slot. Existing unrelated slots are never removed by this UI."><Input value={connection.slot} onChange={(_, d) => update('slot', d.value)} /></Field>
      </> : <Field label="Replication server ID" hint="Choose an ID unique among replication clients."><Input type="number" min={1} value={String(connection.serverId)} onChange={(_, d) => update('serverId', Number(d.value))} /></Field>}
      <Field className="full-width" label="Subscribed table names (comma-separated)" hint="Native graph labels are the table names. Discover the schema, select tables, then validate the final settings.">
        <CsvInput value={connection.tables} onChange={tables => update('tables', tables)} />
      </Field>
    </fieldset>
    <div className="inline-actions"><Button appearance="secondary" disabled={busy} onClick={() => void validate()}>Discover & validate</Button>{busy && <Busy label="Checking connection…" />}</div>
    <ErrorNotice error={error} />
    {schema && <>
      <h4>Database checks</h4>
      {!isCurrent && <p className="warning-text">Settings changed. Revalidate before saving; these checks describe the last tested settings.</p>}
      {!schema.checks.length && <p className="muted">No readiness checks were returned. Readiness has not been established.</p>}
      <div className="checks">{schema.checks.map((check, index) => <div key={`${check.name}-${index}`} className={`check ${check.ok ? 'check-ok' : 'check-failed'}`}>
        <span>{check.ok ? '✓' : '!'}</span><div><strong>{check.name}</strong><p>{check.detail}</p></div>
      </div>)}</div>
      <h4>Tables & result identity</h4>
      {!schema.tables.length && <p className="muted">No tables discovered in this database.</p>}
      {schema.tables.map(table => {
        const selected = connection.tables.includes(table.name);
        const primaryKey = table.columns.filter(column => column.primaryKey).map(column => column.name);
        const override = connection.tableKeys.find(key => key.table === table.name);
        return <div className="schema-table" key={table.name}>
          <Checkbox checked={selected} label={`${table.name} · label ${table.label || table.name}`} disabled={busy} onChange={(_, d) => {
            update('tables', d.checked ? [...connection.tables, table.name] : connection.tables.filter(name => name !== table.name));
            if (!d.checked) setConnection(previous => ({ ...previous, tableKeys: previous.tableKeys.filter(key => key.table !== table.name) }));
          }} />
          <p className="schema-columns">{table.columns.map(column => `${column.name}: ${column.type}${column.primaryKey ? ' [PK]' : ''}`).join(' · ')}</p>
          {selected && <Field label="Key override (optional, comma-separated)" hint={primaryKey.length ? `Discovered primary key: ${primaryKey.join(', ')}` : 'No primary key discovered. An explicit override is required.'}>
            <CsvInput disabled={busy} value={override?.columns || []} onChange={columns => {
              const remaining = connection.tableKeys.filter(key => key.table !== table.name);
              update('tableKeys', columns.length ? [...remaining, { table: table.name, columns }] : remaining);
            }} />
          </Field>}
        </div>;
      })}
      {!!problems.length && <MessageBar intent="warning"><MessageBarBody>{problems.join(' ')}</MessageBarBody></MessageBar>}
      {schema.checks.some(check => !check.ok) && <p className="warning-text">Readiness failures remain visible. Saving stores configuration only; it does not make the source ready.</p>}
    </>}
    <p className="muted small">Connection settings and environment aliases persist across restart. Secrets stay on the backend. Query state is rebuilt; previously active routines need an explicit Resume.</p>
    <Button appearance="primary" disabled={busy || !schema || !isCurrent || !!problems.length || !connection.tables.length} onClick={() => void save()}>Save connection</Button>
  </section>;
}
