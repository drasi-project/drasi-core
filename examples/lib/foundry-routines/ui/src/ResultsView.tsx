import { Table, TableBody, TableCell, TableHeader, TableHeaderCell, TableRow } from '@fluentui/react-components';
import { asObject, cell, JsonView, timestamp } from './shared';
import type { Snapshot } from './types';

export default function ResultsView({ snapshot, preview = false }: { snapshot: Snapshot; preview?: boolean }) {
  const rows = snapshot.rows.map(asObject);
  const columns = [...new Set(rows.flatMap(row => Object.keys(row)))];
  return <div className="results-view">
    <div className="section-heading">
      <h3>{preview ? 'Live preview' : 'Current result set'} <span className="muted small">{rows.length} {rows.length === 1 ? 'row' : 'rows'}</span></h3>
      <span className="muted small">Sequence {snapshot.sequence} · epoch <span title={snapshot.epoch}>{snapshot.epoch || 'not assigned'}</span></span>
    </div>
    {rows.length ? <div className="table-scroll"><Table size="small" aria-label="Current query results">
      <TableHeader><TableRow>{columns.map(column => <TableHeaderCell key={column}>{column}</TableHeaderCell>)}</TableRow></TableHeader>
      <TableBody>{rows.map((row, index) => <TableRow key={index}>{columns.map(column => <TableCell key={column}>{cell(row[column])}</TableCell>)}</TableRow>)}</TableBody>
    </Table></div> : <div className="empty-state compact"><strong>No matching rows</strong><p>The query is waiting for data that matches your condition. An empty result set is not an error.</p></div>}
    <h3 className="timeline-heading">Result changes</h3>
    <p className="muted small">All returned changes are shown, including operations not selected for agent invocation.</p>
    {!snapshot.changes.length ? <p className="muted">No result changes reported in this query epoch.</p> : <div className="timeline">
      {snapshot.changes.map((raw, index) => {
        const change = asObject(raw);
        const operation = cell(change.operation);
        return <article className="timeline-event" key={`${cell(change.sequence)}-${cell(change.diffIndex)}-${index}`}>
          <div className={`event-dot ${operation.toLowerCase()}`} />
          <div className="event-content">
            <div className="section-heading"><strong className={`operation ${operation.toLowerCase()}`}>{operation.toUpperCase()}</strong><time>{timestamp(typeof change.occurredAt === 'string' ? change.occurredAt : undefined)}</time></div>
            <p className="muted small">Key {cell(change.resultKey)}{change.sequence !== undefined ? ` · sequence ${cell(change.sequence)}` : ''}</p>
            <details><summary>Before / after & event details</summary><div className="diff-grid">
              <div><h4>Before</h4><JsonView value={change.before} /></div>
              <div><h4>After</h4><JsonView value={change.after} /></div>
            </div><JsonView value={raw} label="Complete result event" /></details>
          </div>
        </article>;
      })}
    </div>}
  </div>;
}
