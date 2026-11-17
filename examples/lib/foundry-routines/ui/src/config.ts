import type { Connection, Routine, Schema } from './types';

export const csv = (value: string): string[] => [...new Set(value.split(',').map(v => v.trim()).filter(Boolean))];

export function newRoutine(): Routine {
  return {
    id: '', name: '', agent: '', prompt: '', query: '', sources: [], joins: [],
    keyFields: [], operations: ['added'], revision: 0, status: 'Draft', error: null,
  };
}

export function newConnection(): Connection {
  return {
    id: '', name: '', kind: 'postgres', host: 'localhost', port: 5432, database: '',
    user: '', passwordRef: '', tls: 'verify', tables: [], tableKeys: [],
    publication: '', slot: '', serverId: 1,
  };
}

export function serializeConnection(connection: Connection): Connection {
  return {
    id: connection.id,
    name: connection.name.trim(),
    kind: connection.kind,
    host: connection.host.trim(),
    port: Number(connection.port),
    database: connection.database.trim(),
    user: connection.user.trim(),
    passwordRef: connection.passwordRef.trim(),
    tls: connection.tls,
    tables: csv(connection.tables.join(',')),
    tableKeys: connection.tableKeys.map(key => ({
      table: key.table.trim(), columns: csv(key.columns.join(',')),
    })).filter(key => key.table && key.columns.length),
    publication: connection.publication.trim(),
    slot: connection.slot.trim(),
    serverId: Number(connection.serverId),
  };
}

export function connectionProblems(connection: Connection, schema?: Schema | null): string[] {
  const errors: string[] = [];
  if (!connection.name.trim() || !connection.host.trim() || !connection.database.trim() || !connection.user.trim()) {
    errors.push('Name, host, database, and user are required.');
  }
  if (!Number.isInteger(connection.port) || connection.port < 1 || connection.port > 65535) {
    errors.push('Port must be an integer between 1 and 65535.');
  }
  if (!/^FOUNDRY_DEMO_[A-Z0-9_]+$/.test(connection.passwordRef.trim())) {
    errors.push('Password reference must be a FOUNDRY_DEMO_ environment alias, not a password.');
  }
  if (connection.kind === 'mysql' && (!Number.isInteger(connection.serverId) || connection.serverId < 1 || connection.serverId > 4294967295)) {
    errors.push('MySQL requires a unique, positive 32-bit server ID.');
  }
  if (schema) {
    for (const name of connection.tables) {
      const table = schema.tables.find(t => t.name === name);
      if (!table) errors.push(`Selected table "${name}" is not in the discovered schema.`);
      else if (!table.columns.some(c => c.primaryKey) && !connection.tableKeys.some(k => k.table === name && k.columns.length)) {
        errors.push(`"${name}" has no primary key. Configure an explicit key override.`);
      }
    }
    for (const key of connection.tableKeys) {
      const table = schema.tables.find(t => t.name === key.table);
      if (!connection.tables.includes(key.table)) errors.push(`Key override "${key.table}" must belong to a selected table.`);
      for (const column of key.columns) {
        if (!table?.columns.some(c => c.name === column)) errors.push(`Unknown key column "${key.table}.${column}".`);
      }
    }
  }
  return errors;
}

export function routineProblems(routine: Routine, requireAgent = true): string[] {
  const errors: string[] = [];
  if (requireAgent && (!routine.name.trim() || !routine.agent.trim() || !routine.prompt.trim())) {
    errors.push('Enter a name, an existing agent name, and a static prompt.');
  }
  if (!routine.query.trim()) errors.push('Write a continuous Cypher query.');
  if (!routine.sources.length || routine.sources.some(s => !s.labels.length)) {
    errors.push('Select a connection and at least one native table label for every source.');
  }
  const owners = new Set<string>();
  for (const source of routine.sources) for (const label of source.labels) {
    if (owners.has(label)) errors.push(`Label "${label}" must have exactly one source owner.`);
    owners.add(label);
  }
  for (const join of routine.joins) {
    if (!join.name || !join.leftLabel || !join.leftProperty || !join.rightLabel || !join.rightProperty) {
      errors.push('Complete every equality join or remove the unused join.');
    } else if (!owners.has(join.leftLabel) || !owners.has(join.rightLabel)) {
      errors.push(`Join "${join.name}" must use labels allocated to sources.`);
    }
  }
  if (!routine.keyFields.length) errors.push('Enter at least one projected result-key field.');
  if (!routine.operations.length) errors.push('Select at least one result operation.');
  return [...new Set(errors)];
}

export function serializeRoutine(routine: Routine): Routine {
  return {
    ...routine,
    name: routine.name.trim(), agent: routine.agent.trim(),
    sources: routine.sources.map(source => ({ ...source, labels: csv(source.labels.join(',')) })),
    keyFields: csv(routine.keyFields.join(',')),
    joins: routine.joins.map(join => Object.fromEntries(
      Object.entries(join).map(([key, value]) => [key, value.trim()]),
    ) as unknown as Routine['joins'][number]),
    operations: [...routine.operations],
  };
}
