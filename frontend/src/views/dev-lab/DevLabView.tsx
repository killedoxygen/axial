import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { ART_PRESETS, InstanceArt, nextArtSeed, type ArtPreset } from '../../art/InstanceArt';
import { Button, Card, Input, SectionHeading } from '../../ui/Atoms';
import { hashStr } from '../../tokens';
import type { Instance } from '../../types';
import './dev-lab.css';

type LabTab = 'art';

function demoInstance(name: string, versionId: string, seed: number, preset: ArtPreset): Instance {
  return {
    id: `dev-lab-${seed.toString(36)}`,
    name,
    version_id: versionId,
    created_at: new Date(0).toISOString(),
    art_seed: seed,
    art_preset: preset,
  };
}

function parseSeed(value: string): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) return 1;
  return parsed >>> 0 || 1;
}

function ArtWorkbench(): JSX.Element {
  const [name, setName] = useState('Moonlit Forge');
  const [versionId, setVersionId] = useState('1.21.1-fabric');
  const [seed, setSeed] = useState(hashStr('Moonlit Forge:1.21.1-fabric') || 1);
  const [preset, setPreset] = useState<ArtPreset>('aurora');
  const inst = demoInstance(name || 'Untitled instance', versionId || 'unknown', seed, preset);

  const randomize = (): void => {
    setSeed(nextArtSeed(seed ^ Date.now()));
  };

  const derive = (): void => {
    setSeed(hashStr(`${name || 'Untitled instance'}:${versionId || 'unknown'}`) || 1);
  };

  return (
    <div class="cp-dev-lab-stack">
      <Card>
        <SectionHeading
          eyebrow="Procedural art"
          title="Instance identity generator"
          right={(
            <div class="cp-dev-lab-actions">
              <Button variant="secondary" size="sm" icon="refresh" onClick={derive}>Derive</Button>
              <Button size="sm" icon="refresh" onClick={randomize}>Randomize</Button>
            </div>
          )}
        />
        <div class="cp-dev-art-controls">
          <label>
            <span>Name</span>
            <Input value={name} onChange={setName} placeholder="Instance name" />
          </label>
          <label>
            <span>Version hint</span>
            <Input value={versionId} onChange={setVersionId} placeholder="1.21.1-fabric" />
          </label>
          <label>
            <span>Seed</span>
            <Input value={String(seed)} onChange={(value) => setSeed(parseSeed(value))} type="number" />
          </label>
        </div>
        <div class="cp-dev-preset-row" aria-label="Artwork preset">
          {ART_PRESETS.map((item) => (
            <button
              key={item}
              type="button"
              data-active={item === preset}
              onClick={() => setPreset(item)}
            >
              {item}
            </button>
          ))}
        </div>
      </Card>

      <div class="cp-dev-preview-grid">
        <Card padding={12} style={{ overflow: 'hidden' }}>
          <div class="cp-dev-preview-label">Banner</div>
          <InstanceArt instance={inst} aspect="banner" radius={18} className="cp-dev-banner-preview" />
        </Card>
        <Card padding={12}>
          <div class="cp-dev-preview-label">Square</div>
          <InstanceArt instance={inst} aspect="square" radius={18} className="cp-dev-square-preview" />
        </Card>
        <Card padding={12}>
          <div class="cp-dev-preview-label">Thumb</div>
          <div class="cp-dev-thumb-row">
            {[36, 52, 68, 96].map((size) => (
              <InstanceArt
                key={size}
                instance={inst}
                aspect="thumb"
                radius={Math.max(10, Math.round(size * 0.22))}
                style={{ width: size, height: size }}
              />
            ))}
          </div>
        </Card>
      </div>
    </div>
  );
}

export function DevLabView(): JSX.Element {
  const [tab, setTab] = useState<LabTab>('art');
  return (
    <div class="cp-view-page cp-dev-lab">
      <div class="cp-page-header">
        <div>
          <h1>Dev lab</h1>
          <div class="cp-page-sub">Developer-only workbench for internal generators and UI experiments.</div>
        </div>
      </div>

      <div class="cp-dev-tabs">
        <button type="button" data-active={tab === 'art'} onClick={() => setTab('art')}>Art generator</button>
      </div>

      {tab === 'art' && <ArtWorkbench />}
    </div>
  );
}
