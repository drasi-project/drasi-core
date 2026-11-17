import React from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { FluentProvider, webDarkTheme } from '@fluentui/react-components';
import App from '../src/App';
import QuerySetup from '../src/QuerySetup';
import { newConnection, newRoutine } from '../src/config';

const wrap = (child: React.ReactNode) => <FluentProvider theme={webDarkTheme}>{child}</FluentProvider>;
const ok = (value: unknown) => new Response(JSON.stringify(value), { status: 200 });
const schema = { tables: [{ name: 'records', label: 'records', columns: [{ name: 'id', type: 'text', primaryKey: true }] }], checks: [] };
beforeEach(() => {
  vi.stubGlobal('ResizeObserver', class {
    observe() {}
    unobserve() {}
    disconnect() {}
  });
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); });

describe('configuration preview flow', () => {
  it('previews query data, releases the preview on edits, and never saves or activates a routine', async () => {
    const fetcher = vi.fn().mockImplementation((path: string, options: RequestInit) => {
      if (path.includes('/schema')) return Promise.resolve(ok(schema));
      if (path === '/api/query/validate') return Promise.resolve(ok({ valid: true }));
      if (path === '/api/previews') return Promise.resolve(ok({ id: 'isolated' }));
      if (path === '/api/previews/isolated' && options.method === 'DELETE') return Promise.resolve(new Response(null, { status: 204 }));
      if (path === '/api/previews/isolated') return Promise.resolve(ok({ epoch: 'preview', sequence: '18446744073709551615', rows: [{ id: 'visible-condition' }], changes: [] }));
      return Promise.reject(new Error(`Unexpected request: ${path}`));
    });
    vi.stubGlobal('fetch', fetcher);
    const routine = {
      ...newRoutine(), query: 'MATCH (r:records) RETURN r.id AS id',
      sources: [{ connectionId: 'db', labels: ['records'] }], keyFields: ['id'],
    };
    const props = { routine, connections: [{ ...newConnection(), id: 'db', name: 'Database', tables: ['records'] }], revision: 0, onChange: vi.fn(), onConnection: vi.fn(), onClose: vi.fn() };
    const { rerender } = render(wrap(<QuerySetup {...props} />));
    fireEvent.click(screen.getByRole('tab', { name: '3 · Query & preview' }));
    fireEvent.click(screen.getByRole('button', { name: 'Start live preview' }));
    await screen.findByText('visible-condition');
    expect(screen.getByText(/18446744073709551615/)).toBeTruthy();
    rerender(wrap(<QuerySetup {...props} routine={{ ...routine, query: 'MATCH (r:records) RETURN r.id AS id, r.value AS value' }} />));
    await waitFor(() => expect(fetcher.mock.calls.some(([path, options]) => path === '/api/previews/isolated' && options.method === 'DELETE')).toBe(true));
    expect(fetcher.mock.calls.some(([path]) => path.startsWith('/api/routines') || path.includes('/activate'))).toBe(false);
    expect(screen.queryByText('visible-condition')).toBeNull();
  });
});

describe('routines shell', () => {
  it('points to the local env file instead of substituting simulated agents when live configuration is missing', async () => {
    vi.stubGlobal('EventSource', class {
      onopen = null;
      onerror = null;
      addEventListener() {}
      close() {}
    });
    vi.stubGlobal('fetch', vi.fn().mockImplementation((path: string) => {
      if (path === '/api/health') return Promise.resolve(ok({ ready: true, mode: 'live', fixtures: false }));
      if (path === '/api/foundry/status') return Promise.resolve(ok({ mode: 'live', project: null, configured: false, identity: 'Default Azure developer credentials', nativeRoutine: false }));
      if (path === '/api/agents') return Promise.resolve(new Response(JSON.stringify({ message: 'Configure .env' }), { status: 400 }));
      if (path === '/api/connections' || path === '/api/routines') return Promise.resolve(ok([]));
      return Promise.reject(new Error(`Unexpected request: ${path}`));
    }));
    render(wrap(<App />));
    await screen.findByText('Configure live Foundry');
    expect(screen.getByText('examples/lib/foundry-routines/.env')).toBeTruthy();
    expect(screen.queryByText('OFFLINE TEST TRANSPORT')).toBeNull();
  });

  it('shows actual API data, hides fixture actions in non-fixture mode, and keeps agent failures visible', async () => {
    class Events {
      static OPEN = 1;
      readyState = 1;
      onopen: (() => void) | null = null;
      onerror: (() => void) | null = null;
      addEventListener() {}
      close() {}
    }
    vi.stubGlobal('EventSource', Events);
    const fetcher = vi.fn().mockImplementation((path: string) => {
      if (path === '/api/health') return Promise.resolve(ok({ ready: true, mode: 'live', fixtures: false }));
      if (path === '/api/foundry/status') return Promise.resolve(ok({ mode: 'live', project: 'Configured project', configured: true, identity: 'Actual backend developer', nativeRoutine: false }));
      if (path === '/api/agents') return Promise.resolve(new Response(JSON.stringify({ code: 'FORBIDDEN', message: 'Agent listing denied', correlationId: 'agent-correlation' }), { status: 403 }));
      if (path === '/api/connections') return Promise.resolve(ok([]));
      if (path === '/api/routines') return Promise.resolve(ok([{ ...newRoutine(), id: 'r-1', name: 'Actual saved routine', agent: 'existing-agent', revision: 2 }]));
      if (path === '/api/routines/r-1/runs?limit=1') return Promise.resolve(ok([]));
      return Promise.reject(new Error(`Unexpected request: ${path}`));
    });
    vi.stubGlobal('fetch', fetcher);
    render(wrap(<App />));
    await screen.findByRole('button', { name: 'Actual saved routine' });
    expect(screen.getByText('LIVE FOUNDRY TRANSPORT')).toBeTruthy();
    expect(screen.queryByRole('button', { name: 'Demo data' })).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'New routine' }));
    await screen.findByText(/Agent listing denied.*agent-correlation/);
    expect(screen.getAllByText('Actual backend developer').length).toBeGreaterThan(0);
    expect(screen.getByRole('combobox', { name: 'Agent' })).toBeTruthy();
    expect(fetcher.mock.calls.every(([, options]) => options.method === 'GET')).toBe(true);
  });
});
