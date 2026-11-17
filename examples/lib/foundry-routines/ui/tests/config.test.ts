import { describe, expect, it } from 'vitest';
import { connectionProblems, newConnection, newRoutine, routineProblems, serializeConnection, serializeRoutine } from '../src/config';
import type { Schema } from '../src/types';

describe('source serialization and metadata validation', () => {
  const connection = {
    ...newConnection(), name: ' Warehouse ', database: ' records ', user: ' reader ',
    passwordRef: 'FOUNDRY_DEMO_DB_PASSWORD', tables: ['records', 'records'],
    tableKeys: [{ table: 'records', columns: ['record_id', 'tenant_id', 'record_id'] }],
  };
  const schema: Schema = {
    checks: [],
    tables: [{ name: 'records', label: 'records', columns: [
      { name: 'record_id', type: 'text', primaryKey: false },
      { name: 'tenant_id', type: 'text', primaryKey: false },
    ] }],
  };
  it('serializes only the API camelCase fields and an environment reference', () => {
    const result = serializeConnection({ ...connection, password: 'must-not-be-stored' } as typeof connection);
    expect(result).toMatchObject({
      name: 'Warehouse', database: 'records', user: 'reader',
      passwordRef: 'FOUNDRY_DEMO_DB_PASSWORD', tables: ['records'],
      tableKeys: [{ table: 'records', columns: ['record_id', 'tenant_id'] }],
    });
    expect(JSON.stringify(result)).not.toContain('must-not-be-stored');
    expect(connectionProblems(result, schema)).toEqual([]);
  });
  it('rejects literal secrets, unknown tables/keys, and missing identity columns', () => {
    expect(connectionProblems({ ...connection, passwordRef: 'literal password' })).toContain('Password reference must be a FOUNDRY_DEMO_ environment alias, not a password.');
    const errors = connectionProblems({ ...connection, tables: ['records', 'absent'], tableKeys: [{ table: 'records', columns: ['unknown'] }] }, schema);
    expect(errors.some(error => error.includes('absent'))).toBe(true);
    expect(errors.some(error => error.includes('records.unknown'))).toBe(true);
    expect(connectionProblems({ ...connection, tableKeys: [] }, schema).some(error => error.includes('no primary key'))).toBe(true);
  });
});

describe('routine configuration', () => {
  const routine = {
    ...newRoutine(), name: ' Condition ', agent: ' assistant ', prompt: 'Static {{not a template}}',
    query: 'MATCH (r:records) RETURN r.record_id AS id',
    sources: [{ connectionId: 'db', labels: ['records', 'records'] }],
    keyFields: ['id', 'id'],
  };
  it('preserves the static prompt and revision, deduplicating source labels and keys', () => {
    const result = serializeRoutine({ ...routine, revision: 7 });
    expect(result).toMatchObject({
      name: 'Condition', agent: 'assistant', prompt: 'Static {{not a template}}',
      sources: [{ connectionId: 'db', labels: ['records'] }], keyFields: ['id'], revision: 7,
      operations: ['added'],
    });
    expect(routineProblems(result)).toEqual([]);
  });
  it('permits query-only validation without an agent but requires label ownership and an operation', () => {
    const candidate = serializeRoutine({ ...routine, name: '', agent: '', prompt: '' });
    expect(routineProblems(candidate, false)).toEqual([]);
    expect(routineProblems(candidate)).not.toEqual([]);
    const problems = routineProblems({
      ...candidate, operations: [],
      sources: [...candidate.sources, { connectionId: 'other', labels: ['records'] }],
    }, false);
    expect(problems.some(error => error.includes('one source owner'))).toBe(true);
    expect(problems.some(error => error.includes('operation'))).toBe(true);
  });
});
