import type { JSX } from 'preact';
import { useMemo, useState } from 'preact/hooks';
import { Button, Card, Input, Pill, SectionHeading, Segmented } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { versions } from '../../store';
import { addInstance } from '../../actions';
import { navigate } from '../../ui-state';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { Version } from '../../types';

type Channel = 'release' | 'snapshot' | 'legacy';

function channelOf(v: Version): Channel {
  const c = v.lifecycle?.channel;
  if (c === 'stable') return 'release';
  if (c === 'preview' || c === 'experimental') return 'snapshot';
  return 'legacy';
}

function loaderOf(v: Version): string {
  if (!v.loader) return 'vanilla';
  const id = v.loader.component_id;
  if (id.includes('fabric')) return 'fabric';
  if (id.includes('quilt')) return 'quilt';
  if (id.includes('neoforged')) return 'neoforge';
  if (id.includes('minecraftforge')) return 'forge';
  return 'modded';
}

const LOADER_LABELS: Record<string, string> = {
  vanilla: 'Vanilla',
  fabric: 'Fabric',
  quilt: 'Quilt',
  neoforge: 'NeoForge',
  forge: 'Forge',
  modded: 'Modded',
};

export function CreateView(): JSX.Element {
  const theme = useTheme();
  const all = versions.value;
  const [name, setName] = useState('');
  const [channel, setChannel] = useState<Channel>('release');
  const [loader, setLoader] = useState<string>('any');
  const [versionId, setVersionId] = useState<string>('');
  const [query, setQuery] = useState('');
  const [busy, setBusy] = useState(false);

  const loaders = useMemo(() => {
    const set = new Set<string>(['any']);
    for (const v of all) {
      if (v.installed && v.launchable) set.add(loaderOf(v));
    }
    return Array.from(set);
  }, [all]);

  const filtered = useMemo(() => {
    const q = query.toLowerCase();
    return all
      .filter(v => v.installed && v.launchable)
      .filter(v => channelOf(v) === channel)
      .filter(v => loader === 'any' || loaderOf(v) === loader)
      .filter(v => !q
        || v.id.toLowerCase().includes(q)
        || v.minecraft_meta.display_name.toLowerCase().includes(q));
  }, [all, channel, loader, query]);

  const active = all.find(v => v.id === versionId);

  const submit = async (): Promise<void> => {
    const trimmed = name.trim();
    if (!trimmed || !versionId) return;
    setBusy(true);
    try {
      const res: any = await api('POST', '/instances', { name: trimmed, version_id: versionId });
      if (res.error) throw new Error(res.error);
      addInstance(res);
      toast(`Created ${trimmed}`);
      navigate({ name: 'instance', id: res.id });
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`, 'error');
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>New instance</h1>
          <div class="cp-page-sub">Pick a name, a loader, and a version</div>
        </div>
      </div>

      <Card>
        <SectionHeading eyebrow="Identity" title="Name" />
        <Input
          value={name}
          onChange={setName}
          placeholder="Aurora Adventure"
          autoFocus
          onKeyDown={(e) => { if (e.key === 'Enter' && versionId) void submit(); }}
        />
      </Card>

      <Card>
        <SectionHeading
          eyebrow="Filter"
          title="Channel and loader"
          right={<Input value={query} onChange={setQuery} placeholder="Filter versions" icon="search" style={{ width: 240 }} />}
        />
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', alignItems: 'center' }}>
          <Segmented<Channel>
            value={channel}
            onChange={setChannel}
            options={[
              { value: 'release', label: 'Release' },
              { value: 'snapshot', label: 'Snapshot' },
              { value: 'legacy', label: 'Legacy' },
            ]}
          />
          <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
            {loaders.map(l => {
              const activeL = l === loader;
              return (
                <button
                  key={l}
                  onClick={() => setLoader(l)}
                  style={{
                    padding: '4px 12px',
                    borderRadius: 999,
                    fontSize: 12,
                    fontWeight: 600,
                    border: `1px solid ${activeL ? theme.accent.line : theme.n.line}`,
                    background: activeL ? theme.accent.soft : theme.n.surface2,
                    color: activeL ? theme.accent.base : theme.n.text,
                    cursor: 'pointer',
                    fontFamily: 'inherit',
                  }}
                >
                  {l === 'any' ? 'Any' : LOADER_LABELS[l] || l}
                </button>
              );
            })}
          </div>
        </div>
      </Card>

      <Card>
        <SectionHeading eyebrow="Version" title={active ? active.minecraft_meta.display_name || active.id : 'Pick a version'}
          right={active && <Pill tone="accent">{LOADER_LABELS[loaderOf(active)] || 'Vanilla'}</Pill>} />
        {filtered.length === 0 ? (
          <div style={{ color: theme.n.textDim, fontSize: 13, padding: '8px 0' }}>
            Nothing installed that matches. Install a version first, or change the filters above.
          </div>
        ) : (
          <div style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, minmax(180px, 1fr))',
            gap: 6,
            maxHeight: 360,
            overflow: 'auto',
            paddingRight: 4,
          }}>
            {filtered.map(v => {
              const activeV = v.id === versionId;
              return (
                <button
                  key={v.id}
                  onClick={() => setVersionId(v.id)}
                  style={{
                    textAlign: 'left',
                    padding: '10px 12px',
                    borderRadius: theme.r.sm,
                    border: `1px solid ${activeV ? theme.accent.line : theme.n.line}`,
                    background: activeV ? theme.accent.softer : theme.n.surface2,
                    color: activeV ? theme.accent.base : theme.n.text,
                    cursor: 'pointer',
                    display: 'flex',
                    alignItems: 'center',
                    gap: 10,
                    fontFamily: 'inherit',
                  }}
                >
                  <Icon name="tag" size={13} />
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ fontSize: 13, fontWeight: 600, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
                      {v.minecraft_meta.display_name || v.id}
                    </div>
                    <div style={{ fontSize: 11, color: theme.n.textMute, letterSpacing: 0.2 }}>
                      {LOADER_LABELS[loaderOf(v)] || 'Vanilla'} {v.loader?.loader_version ? ` ${v.loader.loader_version}` : ''}
                    </div>
                  </div>
                  {activeV && <Icon name="check" size={14} />}
                </button>
              );
            })}
          </div>
        )}
      </Card>

      <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end', paddingBottom: 10 }}>
        <Button variant="ghost" onClick={() => navigate({ name: 'instances' })} disabled={busy}>Cancel</Button>
        <Button icon="check" onClick={submit} disabled={busy || !name.trim() || !versionId}>
          {busy ? 'Creating…' : 'Create instance'}
        </Button>
      </div>
    </div>
  );
}
