export type Operation = 'added' | 'updated' | 'removed';
export type Mode = 'live' | 'offline';
export interface Health { ready: boolean; mode: Mode; fixtures: boolean }
export interface FoundryStatus {
  mode: Mode;
  project: string | null;
  configured: boolean;
  identity: string;
  nativeRoutine: false;
}
export interface Agent { name: string; kind: string; offline?: boolean }
export interface Connection {
  id: string;
  name: string;
  kind: 'postgres' | 'mysql';
  host: string;
  port: number;
  database: string;
  user: string;
  passwordRef: string;
  tls: 'disabled' | 'verify';
  tables: string[];
  tableKeys: { table: string; columns: string[] }[];
  publication: string;
  slot: string;
  serverId: number;
}
export interface Column { name: string; type: string; primaryKey: boolean }
export interface TableMetadata { name: string; label: string; columns: Column[] }
export interface Schema {
  tables: TableMetadata[];
  checks: { name: string; ok: boolean; detail: string }[];
}
export interface Join {
  name: string;
  leftLabel: string;
  leftProperty: string;
  rightLabel: string;
  rightProperty: string;
}
export interface Routine {
  id: string;
  name: string;
  agent: string;
  prompt: string;
  query: string;
  sources: { connectionId: string; labels: string[] }[];
  joins: Join[];
  keyFields: string[];
  operations: Operation[];
  revision: number;
  status: 'Draft' | 'Starting' | 'Active' | 'Paused' | 'Error' | 'Interrupted';
  error: string | null;
}
export interface Snapshot {
  epoch: string;
  sequence: string;
  rows: unknown[];
  changes: unknown[];
}
export interface Run {
  id: string;
  routineId: string;
  agent: string;
  input: unknown;
  status: 'Queued' | 'Submitting' | 'Running' | 'Completed' | 'Failed' | 'Unknown' | 'Cancelled';
  response: unknown | null;
  error: string | null;
  createdAt: string;
  updatedAt: string;
}
export interface DemoAction { name: string; title: string }
export interface Resource<T> {
  data: T | null;
  error: Error | null;
  loading: boolean;
}
