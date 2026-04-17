import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import type { EnrichedInstance, Version } from '../types';
import {
  selectedInstance, selectedVersion, versions, config, instanceLaunchDrafts,
} from '../store';
import { parseVersionDisplay, formatRelativeTime, versionBadgeInfo } from '../utils';
import { api } from '../api';
import { Sound } from '../sound';
import { updateInstanceInList } from '../actions';
import { toast } from '../toast';
import { ActionArea } from './ActionArea';
import { DetailsPane } from './instance/DetailsPane';
import { SavesPane } from './instance/SavesPane';
import { ModsPane } from './instance/ModsPane';
import { ResourcesPane } from './instance/ResourcesPane';
import { ScreenshotsPane } from './instance/ScreenshotsPane';
import { AdvancedPane } from './instance/AdvancedPane';

function badgeInfo(version: Version | null): { cls: string; text: string } {
  if (version?.inherits_from) return { cls: 'badge-modded', text: 'MOD' };
  return versionBadgeInfo(version);
}

function jvmPresetLabel(preset: string): string | null {
  if (preset === '') return 'Auto JVM';
  if (preset === 'smooth') return 'Smooth GC';
  if (preset === 'performance') return 'Performance GC';
  if (preset === 'ultra_low_latency') return 'Ultra Low Latency';
  if (preset === 'graalvm') return 'GraalVM';
  if (preset === 'legacy') return 'Legacy GC';
  if (preset === 'legacy_pvp') return 'Legacy PvP GC';
  if (preset === 'legacy_heavy') return 'Legacy Heavy GC';
  if (preset === 'aikar') return "Aikar's Flags";
  if (preset === 'zgc') return 'ZGC';
  return null;
}

function MetaDot(): JSX.Element {
  return <span class="meta-dot">{'\u00b7'}</span>;
}

function TabIcon({ tab }: { tab: string }): JSX.Element | null {
  if (tab === 'details') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M12 9h.01" /><path d="M11 12h1v4h1" /></svg>;
  if (tab === 'saves') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 18 0a9 9 0 0 0-18 0" /><path d="M3.6 9h16.8" /><path d="M3.6 15h16.8" /><path d="M11.5 3a17 17 0 0 0 0 18" /><path d="M12.5 3a17 17 0 0 1 0 18" /></svg>;
  if (tab === 'mods') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h3a1 1 0 0 0 1-1v-1a2 2 0 0 1 4 0v1a1 1 0 0 0 1 1h3a1 1 0 0 1 1 1v3a1 1 0 0 0 1 1h1a2 2 0 0 1 0 4h-1a1 1 0 0 0-1 1v3a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-1a2 2 0 0 0-4 0v1a1 1 0 0 1-1 1h-3a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1h1a2 2 0 0 0 0-4h-1a1 1 0 0 1-1-1v-3a1 1 0 0 1 1-1" /></svg>;
  if (tab === 'resource-packs') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21a9 9 0 0 1 0-18c4.97 0 9 3.582 9 8c0 1.06-.474 2.078-1.318 2.828a4.007 4.007 0 0 1-2.682 1.172h-2.5a2 2 0 0 0-1 3.75a1.3 1.3 0 0 1-1.5 1.25" /><path d="M8.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M12.5 7.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /><path d="M16.5 10.5m-1 0a1 1 0 1 0 2 0a1 1 0 1 0-2 0" /></svg>;
  if (tab === 'screenshots') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 7h1a2 2 0 0 0 2-2a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1a2 2 0 0 0 2 2h1a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-14a2 2 0 0 1-2-2v-9a2 2 0 0 1 2-2" /><path d="M9 13a3 3 0 1 0 6 0a3 3 0 0 0-6 0" /></svg>;
  if (tab === 'advanced') return <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 10a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M6 4v4" /><path d="M6 12v8" /><path d="M10 16a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M12 4v10" /><path d="M12 18v2" /><path d="M16 7a2 2 0 1 0 4 0a2 2 0 0 0-4 0" /><path d="M18 4v1" /><path d="M18 9v11" /></svg>;
  return null;
}

type InstanceDetailTab = 'details' | 'saves' | 'mods' | 'resource-packs' | 'screenshots' | 'advanced';

export function InstanceDetail(): JSX.Element | null {
  const inst = selectedInstance.value;
  if (!inst) return null;

  const [javaPath, setJavaPath] = useState(inst.java_path || '');
  const [jvmPreset, setJvmPreset] = useState(inst.jvm_preset || '');
  const [extraJvmArgs, setExtraJvmArgs] = useState(inst.extra_jvm_args || '');
  const [saving, setSaving] = useState(false);
  const [activeTab, setActiveTab] = useState<InstanceDetailTab>('details');

  const version = selectedVersion.value;
  const allVersions = versions.value;
  const cfg = config.value;

  useEffect(() => {
    setJavaPath(inst.java_path || '');
    setJvmPreset(inst.jvm_preset || '');
    setExtraJvmArgs(inst.extra_jvm_args || '');
    setSaving(false);
    setActiveTab('details');
    instanceLaunchDrafts.value = {
      ...instanceLaunchDrafts.value,
      [inst.id]: {
        javaPath: inst.java_path || '',
        jvmPreset: inst.jvm_preset || '',
        extraJvmArgs: inst.extra_jvm_args || '',
        dirty: false,
      },
    };
  }, [inst.id, inst.java_path, inst.jvm_preset, inst.extra_jvm_args]);

  const badge = badgeInfo(version);
  const pd = parseVersionDisplay(inst.version_id, version, allVersions);

  // Build meta parts
  const metaParts: JSX.Element[] = [];
  if (pd.hint) {
    metaParts.push(<span key="ver">{pd.name} <span class="meta-hint">{pd.hint}</span></span>);
  } else {
    metaParts.push(<span key="ver">{pd.name}</span>);
  }
  if (version?.java_major) {
    metaParts.push(<span key="java">Java {version.java_major}</span>);
  }
  const preset: string = inst.jvm_preset || cfg?.jvm_preset || '';
  const presetText = jvmPresetLabel(preset);
  if (presetText) {
    const blocked = preset === 'zgc' && version?.java_major != null && version.java_major < 17;
    if (blocked) {
      metaParts.push(<span key="jvm" style="opacity:.5" title="ZGC requires Java 17+">{presetText}</span>);
    } else {
      metaParts.push(<span key="jvm">{presetText}</span>);
    }
  }
  if (version) {
    metaParts.push(<span key="status">{version.launchable ? 'Ready' : version.status_detail || 'Incomplete'}</span>);
  } else {
    metaParts.push(<span key="status">Version not installed</span>);
  }
  if (inst.last_played_at) {
    const d = new Date(inst.last_played_at);
    if (!isNaN(d.getTime())) {
      metaParts.push(<span key="played">Played {formatRelativeTime(d)}</span>);
    } else {
      metaParts.push(<span key="played">Never played</span>);
    }
  } else {
    metaParts.push(<span key="played">Never played</span>);
  }
  const metaChildren: JSX.Element[] = [];
  for (let i = 0; i < metaParts.length; i++) {
    if (i > 0) metaChildren.push(<MetaDot key={`dot-${i}`} />);
    metaChildren.push(metaParts[i]);
  }

  const isVanilla = !version?.inherits_from;
  const enriched = inst as unknown as EnrichedInstance;

  const tabs: Array<{ key: InstanceDetailTab; label: string }> = [
    { key: 'details', label: 'Details' },
    { key: 'saves', label: 'Saves' },
    { key: 'mods', label: 'Mods' },
    { key: 'resource-packs', label: 'Resources' },
    { key: 'screenshots', label: 'Screenshots' },
    { key: 'advanced', label: 'Advanced' },
  ];

  const saveAdvancedLaunch = async () => {
    if (saving) return;
    setSaving(true);
    try {
      const res = await api('PUT', `/instances/${encodeURIComponent(inst.id)}`, {
        java_path: javaPath.trim(),
        jvm_preset: jvmPreset,
        extra_jvm_args: extraJvmArgs.trim(),
      });
      if (res.error) {
        toast(res.error, 'error');
        return;
      }
      updateInstanceInList(res);
      instanceLaunchDrafts.value = {
        ...instanceLaunchDrafts.value,
        [inst.id]: {
          javaPath: res.java_path || '',
          jvmPreset: res.jvm_preset || '',
          extraJvmArgs: res.extra_jvm_args || '',
          dirty: false,
        },
      };
      toast('Instance launch overrides saved');
    } catch (err) {
      toast(err instanceof Error ? err.message : 'Failed to save instance overrides', 'error');
    } finally {
      setSaving(false);
    }
  };

  const resetAdvancedLaunch = () => {
    setJavaPath('');
    setJvmPreset('');
    setExtraJvmArgs('');
    instanceLaunchDrafts.value = {
      ...instanceLaunchDrafts.value,
      [inst.id]: {
        javaPath: '',
        jvmPreset: '',
        extraJvmArgs: '',
        dirty: !!(inst.java_path || inst.jvm_preset || inst.extra_jvm_args),
      },
    };
    Sound.ui('soft');
  };

  const updateDraft = (next: { javaPath?: string; jvmPreset?: string; extraJvmArgs?: string }) => {
    const nextJavaPath = next.javaPath ?? javaPath;
    const nextJvmPreset = next.jvmPreset ?? jvmPreset;
    const nextExtraJvmArgs = next.extraJvmArgs ?? extraJvmArgs;
    instanceLaunchDrafts.value = {
      ...instanceLaunchDrafts.value,
      [inst.id]: {
        javaPath: nextJavaPath,
        jvmPreset: nextJvmPreset,
        extraJvmArgs: nextExtraJvmArgs,
        dirty: nextJavaPath !== (inst.java_path || '')
          || nextJvmPreset !== (inst.jvm_preset || '')
          || nextExtraJvmArgs !== (inst.extra_jvm_args || ''),
      },
    };
  };

  return (
    <div class="instance-page">
      {/* Hero Section */}
      <div class="instance-hero">
        <div class="instance-hero-info">
          <div class="instance-hero-name-row">
            <h1 class="instance-hero-name" id="detail-id">{inst.name}</h1>
            <span class={`detail-badge ${badge.cls}`} id="detail-badge">{badge.text}</span>
          </div>
          <div class="instance-hero-meta">{metaChildren}</div>
        </div>
      </div>

      {/* Body: tabs + content, vertically centered in remaining space */}
      <div class="instance-body">
        {/* Tab Bar */}
        <div class="instance-tabs" role="tablist" aria-label="Instance detail tabs">
          {tabs.map((tab) => (
            <button
              key={tab.key}
              type="button"
              role="tab"
              aria-selected={activeTab === tab.key}
              class={`instance-tab${activeTab === tab.key ? ' active' : ''}`}
              onClick={() => {
                setActiveTab(tab.key);
                Sound.ui(activeTab === tab.key ? 'soft' : 'click');
              }}
            >
              <TabIcon tab={tab.key} />
              <span class="instance-tab-label">{tab.label}</span>
            </button>
          ))}
        </div>

        {/* Tab Content */}
        <div class="instance-tab-content">
          <div class="instance-tab-pane" key={activeTab}>
            {activeTab === 'details' && <DetailsPane inst={enriched} version={version} />}
            {activeTab === 'saves' && <SavesPane count={enriched.saves_count} />}
            {activeTab === 'mods' && <ModsPane count={enriched.mods_count} isVanilla={isVanilla} />}
            {activeTab === 'resource-packs' && <ResourcesPane count={enriched.resource_count} />}
            {activeTab === 'screenshots' && <ScreenshotsPane />}
            {activeTab === 'advanced' && (
              <AdvancedPane
                inst={enriched}
                cfg={cfg}
                isVanilla={isVanilla}
                javaPath={javaPath}
                jvmPreset={jvmPreset}
                extraJvmArgs={extraJvmArgs}
                saving={saving}
                onJavaPath={(value) => {
                  setJavaPath(value);
                  updateDraft({ javaPath: value });
                }}
                onJvmPreset={(value) => {
                  setJvmPreset(value);
                  updateDraft({ jvmPreset: value });
                }}
                onExtraJvmArgs={(value) => {
                  setExtraJvmArgs(value);
                  updateDraft({ extraJvmArgs: value });
                }}
                onSave={() => { void saveAdvancedLaunch(); }}
                onReset={resetAdvancedLaunch}
              />
            )}
          </div>
        </div>

        {/* Action area always inside body, right below content */}
        <div class="instance-page-actions">
          <ActionArea />
        </div>
      </div>
    </div>
  );
}
