import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import './dev-lab.css';
import { InstanceTile, nextArtSeed } from '../../ui/InstanceVisual';
import { Button, Card, Input, SectionHeading } from '../../ui/Atoms';
import { hashStr } from '../../tokens';
import type { Instance } from '../../types';

type LabTab = 'identity';

function demoInstance(name: string, versionId: string, seed: number): Instance {
  return {
    id: `dev-lab-${seed.toString(36)}`,
    name,
    version_id: versionId,
    created_at: new Date(0).toISOString(),
    art_seed: seed,
  };
}

function parseSeed(value: string): number {
  const parsed = Number.parseInt(value, 10);
  if (!Number.isFinite(parsed) || parsed <= 0) return 1;
  return parsed >>> 0 || 1;
}

function IdentityWorkbench(): JSX.Element {
  const [name, setName] = useState('Moonlit Forge');
  const [versionId, setVersionId] = useState('1.21.1-fabric');
  const [seed, setSeed] = useState(hashStr('Moonlit Forge:1.21.1-fabric') || 1);
  const inst = demoInstance(name || 'Untitled instance', versionId || 'unknown', seed);

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
          title="Instance identity tiles"
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
      </Card>

      <Card padding={12}>
        <div class="cp-dev-preview-label">Tile fallback sizes</div>
        <div class="cp-dev-thumb-row">
          {[36, 52, 68, 96, 160].map((size) => (
            <InstanceTile
              key={size}
              inst={inst}
              radius={Math.max(10, Math.round(size * 0.22))}
              style={{ width: size, height: size }}
            />
          ))}
        </div>
      </Card>
    </div>
  );
}

export function DevLabView(): JSX.Element {
  const [tab, setTab] = useState<LabTab>('identity');
  return (
    <div class="cp-view-page cp-dev-lab">
      <div class="cp-page-header">
        <div>
          <h1>Dev lab</h1>
          <div class="cp-page-sub">Developer-only workbench for internal generators and UI experiments.</div>
        </div>
      </div>

      <div class="cp-dev-tabs">
        <button type="button" data-active={tab === 'identity'} onClick={() => setTab('identity')}>Identity tiles</button>
      </div>

      {tab === 'identity' && <IdentityWorkbench />}
    </div>
  );
}
