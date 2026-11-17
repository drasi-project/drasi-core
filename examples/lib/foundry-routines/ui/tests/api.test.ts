import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, closePreview, request, saveDraft, startPreview } from '../src/api';
import { newRoutine } from '../src/config';

afterEach(() => vi.unstubAllGlobals());
const ok = (value: unknown) => new Response(JSON.stringify(value), { status: 200, headers: { 'Content-Type': 'application/json' } });

describe('query-only preview and draft boundaries', () => {
  it('validates and starts a preview with no routine persistence or activation call', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce(ok({ valid: true })).mockResolvedValueOnce(ok({ id: 'preview-id' })).mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetcher);
    const candidate = { ...newRoutine(), query: 'MATCH (x:records) RETURN x.id AS id' };
    expect(await startPreview(candidate)).toEqual({ id: 'preview-id' });
    await closePreview('preview-id');
    expect(fetcher.mock.calls.map(([path, options]) => [path, options.method])).toEqual([
      ['/api/query/validate', 'POST'], ['/api/previews', 'POST'], ['/api/previews/preview-id', 'DELETE'],
    ]);
    expect(JSON.parse(fetcher.mock.calls[0][1].body).agent).toBe('Query preview (no agent)');
    expect(candidate.agent).toBe('');
  });
  it('does not create a preview after a validation failure and preserves correlation details', async () => {
    const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify({
      code: 'INVALID_QUERY', message: 'Unknown label', correlationId: 'corr-1',
    }), { status: 400 }));
    vi.stubGlobal('fetch', fetcher);
    await expect(startPreview(newRoutine())).rejects.toMatchObject({ message: 'Unknown label', code: 'INVALID_QUERY', correlationId: 'corr-1' });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it('requires affirmative validation instead of a success-shaped HTTP response', async () => {
    const fetcher = vi.fn().mockResolvedValue(ok({ valid: false }));
    vi.stubGlobal('fetch', fetcher);
    await expect(startPreview(newRoutine())).rejects.toMatchObject({ code: 'QUERY_NOT_VALIDATED' });
    expect(fetcher).toHaveBeenCalledTimes(1);
  });
  it('saves new drafts without starting and version-checks edits using PUT', async () => {
    const fetcher = vi.fn().mockImplementation((_path, options) => Promise.resolve(ok(JSON.parse(options.body))));
    vi.stubGlobal('fetch', fetcher);
    await saveDraft(newRoutine());
    await saveDraft({ ...newRoutine(), id: 'routine/id', revision: 3 });
    expect(fetcher.mock.calls.map(([path, options]) => [path, options.method])).toEqual([
      ['/api/routines', 'POST'], ['/api/routines/routine%2Fid', 'PUT'],
    ]);
    expect(JSON.parse(fetcher.mock.calls[0][1].body).id).toBe('');
    expect(JSON.parse(fetcher.mock.calls[1][1].body).revision).toBe(3);
  });
  it('never turns a live network failure into a simulated success', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('Failed to fetch')));
    await expect(request('/api/agents')).rejects.toBeInstanceOf(ApiError);
    await expect(request('/api/agents')).rejects.toMatchObject({ code: 'NETWORK' });
  });
});
