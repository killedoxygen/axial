import type { ComponentChildren, JSX } from 'preact';
import { useState } from 'preact/hooks';
import {
  Button,
  Chip,
  Empty,
  IconButton,
  Input,
  Keys,
  Meter,
  Notice,
  Ring,
  Status,
  Tabs,
  Toggle,
  type TabItem,
} from '../../ui/Atoms';
import { showConfirm } from '../../ui/Dialog';
import { Face, type Eyes, type FaceTone } from '../../ui/Face';
import { Segmented } from '../../ui/Segmented';
import { SelectField } from '../../ui/Select';
import { SelectionCheckbox } from '../../ui/SelectionActionTray';
import { Slider } from '../../ui/Slider';
import { tipProps } from '../../ui/Tip';
import { toast } from '../../toast';

const FACES: Array<{ eyes: Eyes; tone?: FaceTone; name: string; says: string }> = [
  { eyes: 'open', name: 'Awake', says: 'ready' },
  { eyes: 'closed', tone: 'mute', name: 'Asleep', says: 'nothing here' },
  { eyes: 'wide', tone: 'warn', name: 'Wary', says: 'check this' },
  { eyes: 'x', tone: 'err', name: 'Failed', says: 'needs you' },
  { eyes: 'happy', name: 'Done', says: 'finished' },
];
const TABS: TabItem<string>[] = [
  { id: 'mods', label: 'Mods', icon: 'puzzle', count: 9 },
  { id: 'packs', label: 'Resource packs', icon: 'image', count: 2 },
  { id: 'worlds', label: 'Worlds', icon: 'globe' },
  { id: 'logs', label: 'Logs', icon: 'terminal' },
  { id: 'settings', label: 'Settings', icon: 'settings', disabled: true },
];
const FILTERS = ['All mods', 'Enabled', 'Disabled', 'Updates available'].map((label) => ({ value: label, label }));

function Group({ title, children }: { title: string; children: ComponentChildren }): JSX.Element {
  return (
    <section class="cp-spec-group">
      <div class="cp-head">
        <h2>{title}</h2>
      </div>
      {children}
    </section>
  );
}

function Line({ children }: { children: ComponentChildren }): JSX.Element {
  return <div class="cp-spec-line">{children}</div>;
}

/** Every shared control at rest, built from the real components: the sheet the design is checked against. */
export function Primitives(): JSX.Element {
  const [tab, setTab] = useState('mods');
  const [on, setOn] = useState(true);
  const [checked, setChecked] = useState(true);
  const [view, setView] = useState('grid');
  const [filter, setFilter] = useState('All mods');
  const [text, setText] = useState('');
  const [level, setLevel] = useState(34);

  return (
    <div class="cp-spec">
      <Group title="Buttons">
        <Line>
          <Button icon="play">Play</Button>
          <Button variant="secondary" icon="folder">
            Open folder
          </Button>
          <Button variant="soft" icon="arrow-up">
            Update all
          </Button>
          <Button variant="ghost">Dismiss</Button>
          <Button variant="danger" icon="trash">
            Delete
          </Button>
          <Button icon="play" disabled>
            Play
          </Button>
        </Line>
        <Line>
          <Button variant="secondary" size="sm">
            Small
          </Button>
          <Button variant="secondary">Default</Button>
          <Button size="lg" icon="plus">
            New instance
          </Button>
          <Button busy>Installing</Button>
          <Button variant="secondary" busy>
            Preparing
          </Button>
          <Button variant="secondary" size="sm" busy>
            Checking
          </Button>
        </Line>
        <Line>
          <IconButton icon="refresh" tooltip="Refresh" />
          <IconButton icon="folder" tooltip="Open folder" />
          <IconButton icon="stack" tooltip="Group" active />
          <IconButton icon="trash" tooltip="Delete" danger />
          <IconButton icon="dots" tooltip="More" disabled />
        </Line>
      </Group>

      <Group title="Fields">
        <Line>
          <Input value={text} onChange={setText} icon="search" placeholder="Search" />
          <Input value="4096" onChange={() => {}} trailing={<span class="cp-unit">MB</span>} />
          <Input value="Fabric/Lab" onChange={() => {}} invalid />
          <SelectField value={filter} onChange={setFilter} options={FILTERS} ariaLabel="Filter" />
          <SelectField look="quiet" value={filter} onChange={setFilter} options={FILTERS} ariaLabel="Filter" />
          <SelectField value={filter} onChange={setFilter} options={FILTERS} ariaLabel="Filter" disabled />
        </Line>
      </Group>

      <Group title="Choices">
        <Line>
          <Toggle on={on} onChange={() => setOn(!on)} ariaLabel="Switch" />
          <Toggle on={false} onChange={() => {}} ariaLabel="Switch off" disabled />
          <SelectionCheckbox selected={checked} label="Checkbox" onToggle={() => setChecked(!checked)} />
          <SelectionCheckbox selected={false} label="Checkbox off" onToggle={() => {}} />
          <SelectionCheckbox selected mixed label="Checkbox mixed" onToggle={() => {}} />
          <span class="cp-radio" data-on="false" />
          <span class="cp-radio" data-on="true" />
          <Segmented
            options={[
              { value: 'grid', label: 'Grid' },
              { value: 'list', label: 'List' },
            ]}
            value={view}
            onChange={setView}
          />
        </Line>
      </Group>

      <Group title="Tabs">
        <Tabs tabs={TABS} active={tab} onSelect={setTab} />
      </Group>

      <Group title="Chips, keys, slider, progress">
        <Line>
          <Chip>Fabric</Chip>
          <Chip mono>1.21.5</Chip>
          <Chip tone="accent" icon="arrow-up">
            3 updates
          </Chip>
          <Chip tone="warn">Beta</Chip>
          <Chip tone="err">Alpha</Chip>
          <Chip tone="bare" icon="download">
            182M
          </Chip>
          <Keys parts={['⌘', 'K']} />
          <Keys parts={['esc']} />
          <Slider value={level} onChange={setLevel} ariaLabel="Slider" class="cp-spec-wide" />
          <Meter value={62} style={{ width: 200 }} ariaLabel="Progress" />
          <Ring />
          <Ring progress={62} />
          <Ring progress={90} size={24} />
        </Line>
      </Group>

      <Group title="The face says how it ended">
        <Line>
          {FACES.map((face) => (
            <span key={face.name} class="cp-spec-face">
              <Face eyes={face.eyes} tone={face.tone} size={84} />
              <Face eyes={face.eyes} tone={face.tone} />
              <Face eyes={face.eyes} tone={face.tone} size={22} />
              <strong>{face.name}</strong>
              {face.says}
            </span>
          ))}
        </Line>
        <Line>
          <Status eyes="open" tone="mute">
            Ready
          </Status>
          <Status busy tone="accent">
            Preparing
          </Status>
          <Status eyes="closed" tone="mute">
            Queued
          </Status>
          <Status eyes="x" tone="err">
            Failed
          </Status>
          <Status eyes="happy" tone="accent">
            Installed
          </Status>
          <Status>Never played</Status>
        </Line>
      </Group>

      <Group title="Notices, empty, rows">
        <div class="cp-spec-columns">
          <div>
            <Notice tone="warned" title="Java 21 is recommended for 1.21.5">
              <p>This instance is set to Java 17.</p>
            </Notice>
            <Notice tone="error" title="Fabric Lab stopped unexpectedly" onDismiss={() => {}}>
              <p>Exit code 1, 12 seconds after launch.</p>
            </Notice>
          </div>
          <Empty
            title="No screenshots yet"
            actions={
              <Button variant="secondary" icon="folder">
                Open folder
              </Button>
            }
          >
            Press F2 in game to take one.
          </Empty>
          <div>
            <div class="cp-row">
              <Face eyes="x" tone="err" />
              <strong>Simple Voice Chat</strong>
              <Chip>Conflicts with Lithium</Chip>
              <span class="cp-head-fill" />
              <Button variant="secondary" size="sm">
                Review
              </Button>
            </div>
            <div class="cp-row" data-active="true">
              <Face eyes="happy" />
              <strong>Update 9.9.9</strong>
              <Chip>Downloaded</Chip>
              <span class="cp-head-fill" />
              <Button variant="soft" size="sm">
                Restart
              </Button>
            </div>
          </div>
        </div>
      </Group>

      <Group title="Overlays">
        <Line>
          <div class="cp-pop cp-menu">
            <button type="button" class="cp-menu-item">
              Open folder
            </button>
            <button type="button" class="cp-menu-item" data-highlighted>
              Duplicate
            </button>
            <div class="cp-menu-gap" />
            <button type="button" class="cp-menu-item cp-menu-item--danger">
              Delete
            </button>
          </div>
          <Button
            variant="secondary"
            onClick={() =>
              void showConfirm('Its worlds, mods and screenshots are removed from this computer.', {
                title: 'Delete Skyblock Trials?',
                confirmText: 'Delete',
                destructive: true,
              })
            }
          >
            Dialog
          </Button>
          <Button variant="secondary" onClick={() => toast('Sodium 0.6.13 installed')}>
            Toast
          </Button>
          <Button variant="secondary" onClick={() => toast("Couldn't reach Modrinth", 'error')}>
            Error toast
          </Button>
          <Button variant="secondary" {...tipProps(<>Search</>)}>
            Tooltip
          </Button>
        </Line>
      </Group>

      <Group title="Type">
        <Line>
          <h1 class="cp-h1">Fabric Lab</h1>
          <span class="cp-head">
            <h2>Installed mods</h2>
          </span>
          <span class="cp-spec-body">Sodium</span>
          <span class="cp-spec-dim">Rendering engine</span>
          <span class="cp-th">Minecraft</span>
        </Line>
      </Group>
    </div>
  );
}
