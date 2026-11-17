import { useState } from 'react';
import { Button, DrawerBody, DrawerHeader, DrawerHeaderTitle, OverlayDrawer } from '@fluentui/react-components';
import { DismissRegular } from '@fluentui/react-icons';
import { request } from './api';
import { useResource } from './hooks';
import { Busy, ErrorNotice } from './shared';
import type { DemoAction } from './types';

export default function DemoDrawer({ onClose, onRefresh, revision }: { onClose: () => void; onRefresh: () => void; revision: number }) {
  const actions = useResource<DemoAction[]>('/api/demo/actions', revision);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [message, setMessage] = useState('');
  async function execute(action: DemoAction) {
    setBusy(action.name); setError(null); setMessage('');
    try {
      const result = await request<{ ok: boolean }>(`/api/demo/actions/${encodeURIComponent(action.name)}`, 'POST');
      if (!result.ok) throw new Error('The backend did not confirm the fixture mutation.');
      setMessage(`Database mutation completed: ${action.title}. Check live results and runs for its effect.`);
      onRefresh();
    } catch (failure) { setError(failure); }
    finally { setBusy(null); }
  }
  return <OverlayDrawer open position="end" size="medium" onOpenChange={(_, d) => { if (!d.open && !busy) onClose(); }}>
    <DrawerHeader><DrawerHeaderTitle action={<Button appearance="subtle" disabled={!!busy} icon={<DismissRegular />} aria-label="Close demo data" onClick={onClose} />}>Demo data</DrawerHeaderTitle></DrawerHeader>
    <DrawerBody><div className="eyebrow">FIXTURE DATABASES ONLY</div><p className="muted">These predefined actions mutate the configured local fixture databases. They do not call an agent directly. Only a selected result change on an active routine can dispatch.</p>
      <ErrorNotice error={actions.error} /><ErrorNotice error={error} />
      {actions.loading && !actions.data && <Busy label="Loading fixture actions…" />}
      {actions.data?.length === 0 && <p className="muted">No fixture actions are configured.</p>}
      <div className="demo-actions">{actions.data?.map(action => <Button key={action.name} disabled={!!busy} onClick={() => void execute(action)}>{busy === action.name ? 'Applying mutation…' : action.title}</Button>)}</div>
      {message && <p role="status" className="muted">{message}</p>}
      <p className="muted small">Action names and titles come from the backend fixture configuration. SQL is not exposed or editable.</p>
    </DrawerBody>
  </OverlayDrawer>;
}
