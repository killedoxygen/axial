import type { JSX } from 'preact';
import { useMemo, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { Button, Card, IconButton, Input, Meter, Pill, SectionHeading } from '../../ui/Atoms';
import { useTheme } from '../../hooks/use-theme';
import { initialsOf } from '../../ui/Thumb';
import { ART_PRESETS, InstanceArt, artPresetForSeed, artSeedFor, artSeedForPreset, nextArtSeed } from '../../art/InstanceArt';
import { showConfirm } from '../../ui/Dialog';
import { openContextMenu } from '../../ui/ContextMenu';
import { instances, runningSessions, versions } from '../../store';
import { navigate } from '../../ui-state';
import { addInstance, removeInstance, selectInstance, updateInstanceInList } from '../../actions';
import { launchGame, killGame } from '../../launch';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';
import type { EnrichedInstance, Version } from '../../types';
import './instance.css';

async function openInstanceFolder(id: string): Promise<void> {
  try {
    const res: any = await api('POST', `/instances/${encodeURIComponent(id)}/open-folder`);
    if (res?.error) toast(`Failed: ${res.error}`, 'error');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function renameInstance(inst: EnrichedInstance): Promise<void> {
  const { prompt } = await import('../../ui/Dialog');
  const next = await prompt('New name for this instance', inst.name, { title: 'Rename instance', confirmText: 'Rename' });
  if (!next || next === inst.name) return;
  try {
    const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, { name: next });
    if (res.error) throw new Error(res.error);
    updateInstanceInList(res);
    toast('Renamed');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function duplicateInstance(inst: EnrichedInstance): Promise<void> {
  try {
    const res: any = await api('POST', '/instances', { name: `${inst.name} copy`, version_id: inst.version_id });
    if (res.error) throw new Error(res.error);
    addInstance(res);
    toast('Duplicated');
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

async function deleteInstanceFlow(inst: EnrichedInstance, onDone?: () => void): Promise<void> {
  const ok = await showConfirm(
    `Delete "${inst.name}" and everything inside it? Saves, mods, and config will be removed.`,
    { title: 'Delete instance', destructive: true, confirmText: 'Delete' },
  );
  if (!ok) return;
  try {
    const res: any = await api('DELETE', `/instances/${encodeURIComponent(inst.id)}`);
    if (res?.error) throw new Error(res.error);
    removeInstance(inst.id);
    toast('Instance deleted');
    onDone?.();
  } catch (err) {
    toast(`Failed: ${errMessage(err)}`, 'error');
  }
}

export { deleteInstanceFlow, duplicateInstance, renameInstance, openInstanceFolder };

type Tab = 'overview' | 'mods' | 'worlds' | 'screenshots' | 'logs' | 'settings';

function fmtRelative(iso?: string): string {
  if (!iso) return 'never';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return 'never';
  const diff = Date.now() - then;
  const m = Math.floor(diff / 60000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}d ago`;
  return new Date(iso).toLocaleDateString();
}

function loaderLabel(v: Version | undefined): string {
  if (!v?.loader) return 'Vanilla';
  const id = v.loader.component_id;
  if (id.includes('fabric')) return 'Fabric';
  if (id.includes('quilt')) return 'Quilt';
  if (id.includes('neoforged')) return 'NeoForge';
  if (id.includes('minecraftforge')) return 'Forge';
  return 'Modded';
}

function ContributionGrid({ weeks = 26 }: { weeks?: number }): JSX.Element {
  // Seeded placeholder, backend doesn't expose session data yet
  const levels = useMemo(() => {
    const arr: number[] = [];
    let state = 0x13572468;
    for (let i = 0; i < weeks * 7; i++) {
      state = Math.imul(state ^ (state >>> 15), 2246822507);
      state = Math.imul(state ^ (state >>> 13), 3266489909);
      const r = ((state >>> 0) % 100) / 100;
      const lv = r < 0.45 ? 0 : r < 0.65 ? 1 : r < 0.85 ? 2 : r < 0.95 ? 3 : 4;
      arr.push(lv);
    }
    return arr;
  }, [weeks]);

  return (
    <div class="cp-contrib" style={{ gridTemplateColumns: `repeat(${weeks}, 12px)` }}>
      {levels.map((lv, i) => (
        <div key={i} class="cp-contrib-cell" data-level={lv} />
      ))}
    </div>
  );
}

function OverviewPane({ inst, onDeleted }: { inst: EnrichedInstance; onDeleted: () => void }): JSX.Element {
  const theme = useTheme();
  const v = versions.value.find(x => x.id === inst.version_id);
  const maxMem = (inst.max_memory_mb ?? 4096) / 1024;

  return (
    <div class="cp-instance-body">
      <div class="cp-instance-main">
        <Card>
          <SectionHeading eyebrow="Activity" title="Recent sessions" />
          <ContributionGrid weeks={26} />
          <div style={{
            display: 'flex', justifyContent: 'space-between', marginTop: 10,
            fontSize: 11, color: theme.n.textMute,
          }}>
            <span>Less</span>
            <div style={{ display: 'flex', gap: 3 }}>
              {[0, 1, 2, 3, 4].map(lv => <div key={lv} class="cp-contrib-cell" data-level={lv} />)}
            </div>
            <span>More</span>
          </div>
        </Card>

        <Card>
          <SectionHeading
            eyebrow="Content"
            title={`${inst.mods_count ?? 0} mods`}
            right={<Button variant="soft" size="sm" icon="plus">Add mod</Button>}
          />
          {(inst.mods_count ?? 0) === 0 ? (
            <div style={{ color: theme.n.textDim, fontSize: 13, padding: '8px 0' }}>
              No mods installed yet.
            </div>
          ) : (
            <div style={{ fontSize: 13, color: theme.n.textDim }}>
              Mod management is available from the Mods tab.
            </div>
          )}
        </Card>

        <Card>
          <SectionHeading eyebrow="Setup" title="Configuration" />
          <dl class="cp-kv">
            <dt>Loader</dt><dd>{loaderLabel(v)} {v?.loader?.loader_version ? `· ${v.loader.loader_version}` : ''}</dd>
            <dt>Minecraft</dt><dd>{v?.minecraft_meta.display_name || 'unknown'}</dd>
            <dt>Java</dt><dd>{inst.java_major ? `Java ${inst.java_major}` : 'bundled'}</dd>
            <dt>Memory</dt><dd>{maxMem} GB</dd>
            <dt>Window</dt><dd>{inst.window_width || 854}×{inst.window_height || 480}</dd>
          </dl>
        </Card>
      </div>

      <div class="cp-instance-side">
        <Card padding={16}>
          <div style={{
            fontSize: 11, fontWeight: 600, textTransform: 'uppercase',
            letterSpacing: 0.8, color: theme.n.textMute, marginBottom: 14,
          }}>Resources</div>
          <div style={{ marginBottom: 14 }}>
            <div class="cp-meta-row">
              <span class="cp-dim">Memory</span>
              <span>{maxMem} GB</span>
            </div>
            <Meter value={Math.min(100, (maxMem / 16) * 100)} />
          </div>
          <div style={{ marginBottom: 14 }}>
            <div class="cp-meta-row">
              <span class="cp-dim">Mods</span>
              <span>{inst.mods_count ?? 0}</span>
            </div>
            <Meter value={Math.min(100, (inst.mods_count ?? 0) * 2)} tone="ok" />
          </div>
          <div>
            <div class="cp-meta-row">
              <span class="cp-dim">Saves</span>
              <span>{inst.saves_count ?? 0}</span>
            </div>
            <Meter value={Math.min(100, (inst.saves_count ?? 0) * 10)} tone="ok" />
          </div>
        </Card>

        <Card padding={6}>
          <button class="cp-quick-action" onClick={() => void openInstanceFolder(inst.id)}>
            <Icon name="folder" size={15} /> Open folder
          </button>
          <button class="cp-quick-action" onClick={() => void renameInstance(inst)}>
            <Icon name="edit" size={15} /> Rename
          </button>
          <button class="cp-quick-action" onClick={() => void duplicateInstance(inst)}>
            <Icon name="copy" size={15} /> Duplicate
          </button>
          <button
            class="cp-quick-action cp-quick-action--danger"
            onClick={() => void deleteInstanceFlow(inst, onDeleted)}
          >
            <Icon name="trash" size={15} /> Delete
          </button>
        </Card>
      </div>
    </div>
  );
}

function PlaceholderPane({ title, hint }: { title: string; hint: string }): JSX.Element {
  const theme = useTheme();
  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <Card padding={32}>
        <div class="cp-empty">
          <h2>{title}</h2>
          <p style={{ color: theme.n.textDim }}>{hint}</p>
        </div>
      </Card>
    </div>
  );
}

function SettingsPane({ inst }: { inst: EnrichedInstance }): JSX.Element {
  const theme = useTheme();
  const initialArtSeed = artSeedFor(inst);
  const [artSeed, setArtSeed] = useState<number>(initialArtSeed);
  const artPreset = artPresetForSeed(artSeed);
  const [maxMem, setMaxMem] = useState<number>((inst.max_memory_mb ?? 4096) / 1024);
  const [minMem, setMinMem] = useState<number>((inst.min_memory_mb ?? 1024) / 1024);
  const [width, setWidth] = useState<number>(inst.window_width ?? 854);
  const [height, setHeight] = useState<number>(inst.window_height ?? 480);
  const [javaPath, setJavaPath] = useState<string>(inst.java_path ?? '');
  const [jvmArgs, setJvmArgs] = useState<string>(inst.extra_jvm_args ?? '');
  const [saving, setSaving] = useState(false);

  const save = async (): Promise<void> => {
    setSaving(true);
    try {
      const res: any = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        max_memory_mb: Math.round(maxMem * 1024),
        min_memory_mb: Math.round(minMem * 1024),
        art_seed: artSeed,
        window_width: width,
        window_height: height,
        java_path: javaPath || null,
        extra_jvm_args: jvmArgs || null,
      });
      if (res?.error) throw new Error(res.error);
      updateInstanceInList(res);
      toast('Saved instance settings');
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`, 'error');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="cp-instance-body" style={{ display: 'block' }}>
      <Card>
        <SectionHeading
          eyebrow="Artwork"
          title="Instance identity"
          right={<Button variant="soft" size="sm" icon="refresh" onClick={() => setArtSeed(nextArtSeed(artSeed))}>Regenerate</Button>}
        />
        <div class="cp-art-settings">
          <InstanceArt
            instance={{ ...inst, art_seed: artSeed }}
            aspect="square"
            radius={theme.r.lg}
            className="cp-art-settings-square"
          />
          <InstanceArt
            instance={{ ...inst, art_seed: artSeed }}
            aspect="banner"
            radius={theme.r.lg}
            className="cp-art-settings-banner"
          />
          <div class="cp-art-preset-list" aria-label="Artwork preset">
            {ART_PRESETS.map((preset) => (
              <button
                key={preset}
                type="button"
                data-active={preset === artPreset}
                onClick={() => setArtSeed((seed) => artSeedForPreset(seed, preset))}
              >
                {preset}
              </button>
            ))}
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Memory" title="JVM heap" />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 20 }}>
          <div>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: theme.n.textDim }}>Max heap</span>
              <span style={{ color: theme.n.text, fontWeight: 700 }}>{maxMem} GB</span>
            </div>
            <input
              type="range"
              min="1" max="32" step="0.5"
              value={String(maxMem)}
              onInput={(e: any) => setMaxMem(parseFloat(e.currentTarget.value))}
              style={{ width: '100%', accentColor: theme.accent.base }}
            />
          </div>
          <div>
            <div style={{ display: 'flex', justifyContent: 'space-between', fontSize: 12, marginBottom: 6 }}>
              <span style={{ color: theme.n.textDim }}>Min heap</span>
              <span style={{ color: theme.n.text, fontWeight: 700 }}>{minMem} GB</span>
            </div>
            <input
              type="range"
              min="0.5" max={maxMem} step="0.5"
              value={String(minMem)}
              onInput={(e: any) => setMinMem(parseFloat(e.currentTarget.value))}
              style={{ width: '100%', accentColor: theme.accent.base }}
            />
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Window" title="Game window" />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))', gap: 16 }}>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Width</div>
            <Input value={String(width)} onChange={(v) => setWidth(parseInt(v, 10) || 0)} />
          </div>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Height</div>
            <Input value={String(height)} onChange={(v) => setHeight(parseInt(v, 10) || 0)} />
          </div>
        </div>
      </Card>
      <div style={{ height: 16 }} />
      <Card>
        <SectionHeading eyebrow="Advanced" title="Java runtime" />
        <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Java path override</div>
            <Input value={javaPath} onChange={setJavaPath} placeholder="Leave blank to use bundled Java" />
          </div>
          <div>
            <div style={{ fontSize: 12, color: theme.n.textDim, marginBottom: 6 }}>Extra JVM args</div>
            <Input value={jvmArgs} onChange={setJvmArgs} placeholder="-Dfoo=bar -Xss2m" />
          </div>
        </div>
      </Card>
      <div style={{ marginTop: 16, display: 'flex', justifyContent: 'flex-end' }}>
        <Button onClick={save} disabled={saving}>{saving ? 'Saving…' : 'Save settings'}</Button>
      </div>
    </div>
  );
}


export function InstanceDetailView({ id }: { id: string }): JSX.Element {
  const theme = useTheme();
  const inst = instances.value.find(i => i.id === id) as EnrichedInstance | undefined;
  const [tab, setTab] = useState<Tab>('overview');
  const running = inst ? !!runningSessions.value[inst.id] : false;

  if (!inst) {
    return (
      <div class="cp-view-page">
        <div class="cp-empty">
          <Icon name="cube" size={36} color="var(--text-mute)" />
          <h2>Instance not found</h2>
          <p>That instance might have been deleted.</p>
          <Button icon="chevron-left" onClick={() => navigate({ name: 'instances' })}>Back to instances</Button>
        </div>
      </div>
    );
  }

  const v = versions.value.find(x => x.id === inst.version_id);
  const tabs: Tab[] = ['overview', 'mods', 'worlds', 'screenshots', 'logs', 'settings'];

  const onPlay = (): void => {
    selectInstance(inst.id);
    void launchGame();
  };
  const onStop = (): void => { void killGame(); };

  return (
    <div style={{ display: 'flex', flexDirection: 'column' }}>
      <div class="cp-instance-hero">
        <InstanceArt instance={inst} aspect="banner" className="cp-instance-hero-art" />
        <div class="cp-instance-hero-gradient" />
        <div class="cp-instance-hero-body">
          <div class="cp-instance-avatar" style={{ borderRadius: theme.r.lg }}>
            <InstanceArt instance={inst} aspect="square" radius={theme.r.lg} />
            <span>{initialsOf(inst.name)}</span>
          </div>
          <div style={{ flex: 1, minWidth: 240 }}>
            <div class="cp-instance-heading-pills">
              <Pill>{loaderLabel(v)}</Pill>
              <Pill>MC {v?.minecraft_meta.display_hint || 'unknown'}</Pill>
              {running && <Pill tone="accent" icon="play">Running</Pill>}
            </div>
            <h1 class="cp-instance-title">{inst.name}</h1>
            <div class="cp-instance-meta">
              <span>{inst.mods_count ?? 0} mods</span>
              <span>·</span>
              <span>{inst.saves_count ?? 0} saves</span>
              <span>·</span>
              <span>last {fmtRelative(inst.last_played_at)}</span>
            </div>
          </div>
          <div class="cp-instance-hero-actions">
            <IconButton icon="folder" tooltip="Open folder" variant="overlay" size={40}
              onClick={() => void openInstanceFolder(inst.id)} />
            <IconButton icon="edit" tooltip="Rename" variant="overlay" size={40}
              onClick={() => void renameInstance(inst)} />
            <IconButton icon="dots" tooltip="More" variant="overlay" size={40}
              onClick={(e) => openContextMenu(e, [
                { icon: 'folder', label: 'Open folder', onSelect: () => void openInstanceFolder(inst.id) },
                { icon: 'copy', label: 'Duplicate', onSelect: () => void duplicateInstance(inst) },
                { icon: 'edit', label: 'Rename', onSelect: () => void renameInstance(inst) },
                { label: '', onSelect: () => {}, divider: true },
                { icon: 'trash', label: 'Delete', onSelect: () => void deleteInstanceFlow(inst, () => navigate({ name: 'instances' })), danger: true },
              ])} />
            {running ? (
              <Button variant="danger" icon="stop" size="lg" onClick={onStop}>Stop</Button>
            ) : (
              <Button icon="play" size="lg" onClick={onPlay}>Play now</Button>
            )}
          </div>
        </div>
      </div>

      <div class="cp-instance-tabs">
        {tabs.map(t => (
          <button key={t} data-active={tab === t} onClick={() => setTab(t)}>
            {t[0].toUpperCase() + t.slice(1)}
          </button>
        ))}
      </div>

      {tab === 'overview' && <OverviewPane inst={inst} onDeleted={() => navigate({ name: 'instances' })} />}
      {tab === 'mods' && (
        <PlaceholderPane
          title={inst.mods_count ? `${inst.mods_count} mods installed` : 'No mods installed'}
          hint="Drop jar files into the instance folder for now, in app mod management is on the roadmap"
        />
      )}
      {tab === 'worlds' && (
        <PlaceholderPane
          title={inst.saves_count ? `${inst.saves_count} saves` : 'No saves yet'}
          hint="World list and last played times will live here once the backend exposes them"
        />
      )}
      {tab === 'screenshots' && (
        <PlaceholderPane
          title="Screenshots"
          hint="Minecraft drops screenshots into the instance folder, we'll surface them here next"
        />
      )}
      {tab === 'logs' && (
        <PlaceholderPane
          title="Logs"
          hint="Launch logs stream in the main launcher surface for now"
        />
      )}
      {tab === 'settings' && <SettingsPane inst={inst} />}
    </div>
  );
}
