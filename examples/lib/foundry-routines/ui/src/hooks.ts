import { useCallback, useEffect, useRef, useState } from 'react';
import { request } from './api';
import type { Resource } from './types';

export function useResource<T>(path: string | null, revision: number): Resource<T> {
  const [resource, setResource] = useState<Resource<T>>({ data: null, error: null, loading: !!path });
  const refresh = useRef<() => void>(() => {});
  useEffect(() => {
    let alive = true;
    let running = false;
    let again = false;
    const controller = new AbortController();
    setResource({ data: null, error: null, loading: !!path });
    async function load() {
      if (!path) return;
      if (running) { again = true; return; }
      running = true;
      setResource(previous => ({ ...previous, loading: true }));
      try {
        const data = await request<T>(path, 'GET', undefined, controller.signal);
        if (alive) setResource({ data, error: null, loading: false });
      } catch (error) {
        if (alive) setResource(previous => ({ ...previous, error: error as Error, loading: false }));
      } finally {
        running = false;
        if (alive && again) { again = false; void load(); }
      }
    }
    refresh.current = () => { void load(); };
    void load();
    return () => { alive = false; controller.abort(); refresh.current = () => {}; };
  }, [path]);
  useEffect(() => { if (revision) refresh.current(); }, [revision, path]);
  return resource;
}

export function useLiveRefresh() {
  const [revision, setRevision] = useState(0);
  const [stream, setStream] = useState<'connecting' | 'connected' | 'disconnected'>('connecting');
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const refresh = useCallback(() => {
    if (timer.current) return;
    timer.current = setTimeout(() => { timer.current = null; setRevision(v => v + 1); }, 300);
  }, []);
  useEffect(() => {
    const events = new EventSource('/api/events');
    events.addEventListener('refresh', refresh);
    events.onopen = () => { setStream('connected'); refresh(); };
    events.onerror = () => { setStream('disconnected'); };
    const fallback = setInterval(() => {
      if (events.readyState !== EventSource.OPEN) refresh();
    }, 15_000);
    return () => {
      events.close();
      clearInterval(fallback);
      if (timer.current) clearTimeout(timer.current);
    };
  }, [refresh]);
  return { revision, stream, refresh };
}
