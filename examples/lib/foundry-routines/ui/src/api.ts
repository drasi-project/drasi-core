import type { Routine, Snapshot } from './types';

export class ApiError extends Error {
  constructor(
    message: string,
    public readonly code: string,
    public readonly correlationId?: string,
    public readonly status?: number,
  ) { super(message); this.name = 'ApiError'; }
}

export function errorMessage(error: unknown): string {
  if (error instanceof ApiError) {
    return `${error.message} [${error.code}]${error.correlationId ? ` · Correlation: ${error.correlationId}` : ''}`;
  }
  return error instanceof Error ? error.message : 'The request failed.';
}

export async function request<T>(path: string, method = 'GET', body?: unknown, signal?: AbortSignal): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      method,
      headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
      credentials: 'same-origin',
      cache: 'no-store',
    });
  } catch (error) {
    if (signal?.aborted) throw error;
    throw new ApiError('Cannot reach the local backend. Check that it is running on localhost:8780.', 'NETWORK');
  }
  if (!response.ok) {
    const detail = await response.json().catch(() => null);
    throw new ApiError(
      detail?.message || `Request failed (${response.status}).`,
      detail?.code || 'HTTP_ERROR',
      detail?.correlationId,
      response.status,
    );
  }
  if (response.status === 204) return undefined as T;
  const text = await response.text();
  if (!text) return undefined as T;
  try { return JSON.parse(text) as T; }
  catch { throw new ApiError('The backend returned an unreadable response.', 'INVALID_RESPONSE'); }
}

export const routinePath = (id: string) => `/api/routines/${encodeURIComponent(id)}`;
export const previewPath = (id: string) => `/api/previews/${encodeURIComponent(id)}`;

export async function validateQuery(routine: Routine): Promise<void> {
  const validation = await request<{ valid: boolean }>('/api/query/validate', 'POST', {
    ...routine, agent: routine.agent.trim() || 'Query preview (no agent)',
  });
  if (validation?.valid !== true) throw new ApiError('Query validation was not confirmed by the backend.', 'QUERY_NOT_VALIDATED');
}

// Query-only validation is intentionally independent of saving and activation.
export async function startPreview(routine: Routine): Promise<{ id: string }> {
  const candidate = { ...routine, agent: routine.agent.trim() || 'Query preview (no agent)' };
  await validateQuery(candidate);
  const preview = await request<{ id: string }>('/api/previews', 'POST', candidate);
  if (!preview?.id) throw new ApiError('The backend did not return a preview identifier.', 'PREVIEW_NOT_CREATED');
  return preview;
}

export const readPreview = (id: string) => request<Snapshot>(previewPath(id));
export const closePreview = (id: string) => request<void>(previewPath(id), 'DELETE');
export const saveDraft = (routine: Routine) => request<Routine>(
  routine.id ? routinePath(routine.id) : '/api/routines',
  routine.id ? 'PUT' : 'POST',
  routine,
);
