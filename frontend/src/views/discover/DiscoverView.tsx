import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Button, Card, Input, Pill } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { instances } from '../../store';
import { toast } from '../../toast';
import { getContentDetail, installContent, planContent, searchContent } from '../../content';
import type { CanonicalContent, ContentDetail, ContentKind, ResolutionPlan } from '../../types-content';
import type { EnrichedInstance } from '../../types-instance';

const KIND_TABS: { kind: ContentKind; label: string }[] = [
  { kind: 'mod', label: 'Mods' },
  { kind: 'modpack', label: 'Modpacks' },
  { kind: 'resource_pack', label: 'Resource packs' },
  { kind: 'shader_pack', label: 'Shaders' },
];

const SEARCH_DEBOUNCE_MS = 300;

function formatCount(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

function formatBytes(bytes: number): string {
  if (!bytes) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${value.toFixed(value >= 10 || exponent === 0 ? 0 : 1)} ${units[exponent]}`;
}

function ContentCard({
  item,
  onOpen,
}: {
  item: CanonicalContent;
  onOpen: (item: CanonicalContent) => void;
}): JSX.Element {
  return (
    <button class="cp-discover-card" onClick={() => onOpen(item)}>
      <div class="cp-discover-card-icon" aria-hidden="true">
        {item.icon_url ? <img src={item.icon_url} alt="" loading="lazy" /> : <Icon name="puzzle" size={22} />}
      </div>
      <div class="cp-discover-card-body">
        <div class="cp-discover-card-title" title={item.title}>
          {item.title}
        </div>
        {item.author && <div class="cp-discover-card-author">by {item.author}</div>}
        <p class="cp-discover-card-summary">{item.summary}</p>
        <div class="cp-discover-card-meta">
          <span>
            <Icon name="download" size={12} /> {formatCount(item.downloads)}
          </span>
          {item.categories.slice(0, 2).map((category) => (
            <span key={category} class="cp-discover-tag">
              {category}
            </span>
          ))}
        </div>
      </div>
    </button>
  );
}

function InstallPanel({ item }: { item: CanonicalContent }): JSX.Element {
  const moddedInstances = useMemo(
    () => (instances.value as EnrichedInstance[]).filter((instance) => instance.version_display.supports_mods),
    [instances.value],
  );
  const [instanceId, setInstanceId] = useState<string>(() => moddedInstances[0]?.id ?? '');
  const [plan, setPlan] = useState<ResolutionPlan | null>(null);
  const [planning, setPlanning] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const installable = item.kind === 'mod';

  useEffect(() => {
    if (!installable || !instanceId) {
      setPlan(null);
      return;
    }
    let active = true;
    setPlanning(true);
    setError(null);
    planContent(instanceId, [{ canonical_id: item.canonical_id, kind: item.kind }])
      .then((resolved) => {
        if (active) setPlan(resolved);
      })
      .catch((err) => {
        if (active) setError(err?.message || 'Could not build an install plan.');
      })
      .finally(() => {
        if (active) setPlanning(false);
      });
    return () => {
      active = false;
    };
  }, [instanceId, item.canonical_id, installable]);

  if (!installable) {
    return (
      <div class="cp-discover-install cp-discover-install--muted">
        Installing {KIND_TABS.find((tab) => tab.kind === item.kind)?.label.toLowerCase() ?? 'this content'} into
        instances is coming soon.
      </div>
    );
  }

  if (moddedInstances.length === 0) {
    return (
      <div class="cp-discover-install cp-discover-install--muted">
        Create a modded instance (Fabric, Forge, NeoForge, or Quilt) to add mods.
      </div>
    );
  }

  const install = (): void => {
    if (!instanceId || installing) return;
    setInstalling(true);
    setError(null);
    installContent(instanceId, [{ canonical_id: item.canonical_id, kind: item.kind }])
      .then(() => {
        toast(`Added ${item.title}`, 'success');
      })
      .catch((err) => {
        setError(err?.message || 'Install failed.');
        toast(err?.message || 'Install failed', 'error');
      })
      .finally(() => setInstalling(false));
  };

  const toInstall = plan?.items.filter((planItem) => !planItem.already_installed || planItem.update) ?? [];
  const dependencyCount = toInstall.filter((planItem) => planItem.reason === 'dependency').length;

  return (
    <div class="cp-discover-install">
      <label class="cp-discover-install-label">
        Add to instance
        <select
          class="cp-discover-select"
          value={instanceId}
          onChange={(e) => setInstanceId((e.target as HTMLSelectElement).value)}
        >
          {moddedInstances.map((instance) => (
            <option key={instance.id} value={instance.id}>
              {instance.name} · {instance.version_display.summary_label}
            </option>
          ))}
        </select>
      </label>

      {planning && <div class="cp-discover-plan-note">Checking compatibility…</div>}

      {plan && !planning && (
        <div class="cp-discover-plan">
          {plan.conflicts.map((conflict, index) => (
            <div key={index} class="cp-discover-conflict">
              <Icon name="alert" size={13} /> {conflict.detail}
            </div>
          ))}
          {toInstall.length === 0 && plan.conflicts.length === 0 && (
            <div class="cp-discover-plan-note">Already up to date in this instance.</div>
          )}
          {toInstall.length > 0 && (
            <div class="cp-discover-plan-note">
              {toInstall.length} file{toInstall.length === 1 ? '' : 's'}
              {dependencyCount > 0 ? ` (incl. ${dependencyCount} dependenc${dependencyCount === 1 ? 'y' : 'ies'})` : ''}
              {plan.total_download_bytes > 0 ? ` · ${formatBytes(plan.total_download_bytes)}` : ''}
            </div>
          )}
        </div>
      )}

      {error && <div class="cp-discover-conflict">{error}</div>}

      <Button icon="download" onClick={install} disabled={installing || planning || !instanceId} full>
        {installing ? 'Installing…' : toInstall.length === 0 && plan ? 'Reinstall' : 'Install'}
      </Button>
    </div>
  );
}

function DetailModal({ item, onClose }: { item: CanonicalContent; onClose: () => void }): JSX.Element {
  const [detail, setDetail] = useState<ContentDetail | null>(null);

  useEffect(() => {
    let active = true;
    setDetail(null);
    getContentDetail(item.canonical_id)
      .then((resolved) => {
        if (active) setDetail(resolved);
      })
      .catch(() => {});
    return () => {
      active = false;
    };
  }, [item.canonical_id]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div class="cp-discover-overlay" onClick={onClose}>
      <div class="cp-discover-sheet" onClick={(e) => e.stopPropagation()}>
        <div class="cp-discover-sheet-head">
          <div class="cp-discover-card-icon cp-discover-sheet-icon" aria-hidden="true">
            {item.icon_url ? <img src={item.icon_url} alt="" /> : <Icon name="puzzle" size={26} />}
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <h2 title={item.title}>{item.title}</h2>
            {item.author && <div class="cp-discover-card-author">by {item.author}</div>}
          </div>
          <button class="cp-discover-close" onClick={onClose} aria-label="Close">
            <Icon name="x" size={16} />
          </button>
        </div>

        <div class="cp-discover-sheet-meta">
          <Pill icon="download">{formatCount(item.downloads)}</Pill>
          {item.categories.slice(0, 4).map((category) => (
            <span key={category} class="cp-discover-tag">
              {category}
            </span>
          ))}
        </div>

        <p class="cp-discover-sheet-summary">{item.summary}</p>

        <InstallPanel item={item} />

        {detail && detail.versions.length > 0 && (
          <div class="cp-discover-versions">
            <div class="cp-discover-versions-head">Recent versions</div>
            {detail.versions.slice(0, 5).map((version) => (
              <div key={version.id} class="cp-discover-version-row">
                <span class="cp-discover-version-name" title={version.name}>
                  {version.version_number}
                </span>
                <span class="cp-discover-version-loaders">{version.loaders.join(', ')}</span>
                <span class="cp-discover-version-channel" data-channel={version.channel}>
                  {version.channel}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

export function DiscoverView(): JSX.Element {
  const [kind, setKind] = useState<ContentKind>('mod');
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<CanonicalContent[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CanonicalContent | null>(null);
  const requestId = useRef(0);

  useEffect(() => {
    const id = ++requestId.current;
    setLoading(true);
    setError(null);
    const timer = window.setTimeout(() => {
      searchContent({ kind, query: query.trim() || undefined, limit: 40 })
        .then((page) => {
          if (id === requestId.current) setResults(page.items);
        })
        .catch((err) => {
          if (id === requestId.current) setError(err?.message || 'Could not load content.');
        })
        .finally(() => {
          if (id === requestId.current) setLoading(false);
        });
    }, SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [kind, query]);

  return (
    <div class="cp-view-page">
      <div class="cp-page-header">
        <div>
          <h1>Discover</h1>
          <div class="cp-page-sub">Browse mods, packs, and shaders from Modrinth and add them to your instances.</div>
        </div>
      </div>

      <div class="cp-discover-controls">
        <div class="cp-discover-tabs" role="tablist">
          {KIND_TABS.map((tab) => (
            <button
              key={tab.kind}
              class="cp-discover-tab"
              role="tab"
              aria-selected={kind === tab.kind}
              data-active={kind === tab.kind}
              onClick={() => setKind(tab.kind)}
            >
              {tab.label}
            </button>
          ))}
        </div>
        <Input value={query} onChange={setQuery} icon="search" placeholder="Search content" style={{ maxWidth: 320 }} />
      </div>

      {error && (
        <Card padding={20}>
          <div class="cp-discover-empty">{error}</div>
        </Card>
      )}

      {!error && loading && results.length === 0 && (
        <div class="cp-discover-empty cp-discover-empty--pad">Loading…</div>
      )}

      {!error && !loading && results.length === 0 && (
        <div class="cp-discover-empty cp-discover-empty--pad">No results. Try a different search.</div>
      )}

      {results.length > 0 && (
        <div class="cp-discover-grid" data-loading={loading}>
          {results.map((item) => (
            <ContentCard key={item.canonical_id} item={item} onOpen={setSelected} />
          ))}
        </div>
      )}

      {selected && <DetailModal item={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}
