import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useLiveRefresh, useResource } from '../src/hooks';

afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); });

describe('authoritative refresh coalescing', () => {
  it('coalesces simultaneous resource refreshes into one follow-up fetch', async () => {
    let resolveFirst!: (response: Response) => void;
    const first = new Promise<Response>(resolve => { resolveFirst = resolve; });
    const fetcher = vi.fn().mockReturnValueOnce(first).mockResolvedValue(new Response(JSON.stringify({ sequence: 'latest' })));
    vi.stubGlobal('fetch', fetcher);
    const { result, rerender } = renderHook(({ revision }) => useResource<{ sequence: string }>('/api/results', revision), { initialProps: { revision: 0 } });
    rerender({ revision: 1 });
    rerender({ revision: 2 });
    rerender({ revision: 3 });
    expect(fetcher).toHaveBeenCalledTimes(1);
    await act(async () => resolveFirst(new Response(JSON.stringify({ sequence: 'old' }))));
    await waitFor(() => expect(result.current.data?.sequence).toBe('latest'));
    expect(fetcher).toHaveBeenCalledTimes(2);
  });
  it('refetches on SSE reconnect, coalesces storms, and closes the stream on unmount', () => {
    vi.useFakeTimers();
    let events!: Events;
    class Events {
      static OPEN = 1;
      readyState = 1;
      onopen: (() => void) | null = null;
      onerror: (() => void) | null = null;
      listener: (() => void) | null = null;
      close = vi.fn();
      constructor() { events = this; }
      addEventListener(_type: string, listener: () => void) { this.listener = listener; }
    }
    vi.stubGlobal('EventSource', Events);
    const { result, unmount } = renderHook(useLiveRefresh);
    act(() => {
      events.onopen?.();
      for (let i = 0; i < 30; i++) events.listener?.();
      vi.advanceTimersByTime(300);
    });
    expect(result.current.revision).toBe(1);
    act(() => { events.onerror?.(); events.onopen?.(); vi.advanceTimersByTime(300); });
    expect(result.current.revision).toBe(2);
    expect(result.current.stream).toBe('connected');
    unmount();
    expect(events.close).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);
  });
});
