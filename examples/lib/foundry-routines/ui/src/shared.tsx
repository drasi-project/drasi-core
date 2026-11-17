import { useEffect, useRef, useState } from 'react';
import { Badge, Input, MessageBar, MessageBarBody, Spinner } from '@fluentui/react-components';
import { csv } from './config';
import type { Mode } from './types';
import { errorMessage } from './api';

export function ErrorNotice({ error }: { error: unknown }) {
  if (!error) return null;
  return <MessageBar intent="error" layout="multiline" className="error-notice"><MessageBarBody>{errorMessage(error)}</MessageBarBody></MessageBar>;
}

export function TransportBadge({ mode }: { mode: Mode | undefined }) {
  return <Badge appearance="tint" color={mode === 'offline' ? 'warning' : mode === 'live' ? 'success' : 'informative'}>
    {mode === 'offline' ? 'OFFLINE TEST TRANSPORT' : mode === 'live' ? 'LIVE FOUNDRY TRANSPORT' : 'TRANSPORT UNKNOWN'}
  </Badge>;
}

export function StatusBadge({ status }: { status: string }) {
  const color = ['Error', 'Failed'].includes(status) ? 'danger'
    : ['Active', 'Completed'].includes(status) ? 'success'
    : ['Interrupted', 'Unknown', 'Paused'].includes(status) ? 'warning'
    : 'informative';
  return <Badge appearance="tint" color={color}>{status}</Badge>;
}

export function Busy({ label = 'Loading…' }: { label?: string }) {
  return <div className="loading"><Spinner size="tiny" label={label} /></div>;
}

export function JsonView({ value, label }: { value: unknown; label?: string }) {
  return <pre className="json" aria-label={label}>{value == null ? 'null' : JSON.stringify(value, null, 2)}</pre>;
}

export function timestamp(value: string | undefined): string {
  if (!value) return '—';
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

export function asObject(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : { value };
}

export function cell(value: unknown): string {
  return value == null ? '—' : typeof value === 'object' ? JSON.stringify(value) : String(value);
}

export function CsvInput({ value, onChange, disabled = false }: {
  value: string[];
  onChange: (value: string[]) => void;
  disabled?: boolean;
}) {
  const [text, setText] = useState(value.join(', '));
  const focused = useRef(false);
  const serialized = value.join(', ');
  useEffect(() => { if (!focused.current) setText(serialized); }, [serialized]);
  return <Input disabled={disabled} value={text} onFocus={() => { focused.current = true; }}
    onBlur={() => { focused.current = false; setText(serialized); }}
    onChange={(_, data) => { setText(data.value); onChange(csv(data.value)); }} />;
}
