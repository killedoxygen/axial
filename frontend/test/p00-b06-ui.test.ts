import assert from 'node:assert/strict';
import test from 'node:test';
import { toChildArray, type VNode } from 'preact';

import type { Version } from '../src/types-version';
import { Button } from '../src/ui/Atoms';
import { SelectionActionTray } from '../src/ui/SelectionActionTray';
import { minecraftVersionLabel } from '../src/version-display';

function version(overrides: Partial<Version> = {}): Version {
  return {
    subject_kind: 'installed_version',
    id: 'version-id',
    raw_kind: 'release',
    minecraft_meta: {
      family: 'release',
      base_id: 'base-id',
      effective_version: 'effective-version',
      variant_of: '',
      variant_kind: '',
      display_name: 'display-name',
      display_hint: 'display-hint',
    },
    lifecycle: {
      channel: 'stable',
      labels: ['release'],
      default_rank: 0,
      badge_text: 'REL',
      provider_terms: [],
    },
    launchable: true,
    installed: true,
    status: 'ready',
    ...overrides,
  };
}

test('Minecraft version labels preserve the exact normalized precedence and fallback bytes', () => {
  assert.equal(minecraftVersionLabel(null), 'unknown');
  assert.equal(minecraftVersionLabel(undefined, 'custom-fallback'), 'custom-fallback');
  assert.equal(minecraftVersionLabel(version({ inherits_from: ' 1.20.1 ' })), '1.20.1');
  assert.equal(minecraftVersionLabel(version({ inherits_from: '   ' })), 'effective-version');

  const fields = ['effective_version', 'base_id', 'display_name', 'display_hint'] as const;
  for (const [index, field] of fields.entries()) {
    const minecraftMeta = version().minecraft_meta;
    for (const earlier of fields.slice(0, index)) minecraftMeta[earlier] = '';
    minecraftMeta[field] = `selected-${field}`;
    assert.equal(minecraftVersionLabel(version({ minecraft_meta: minecraftMeta })), `selected-${field}`);
  }

  const emptyMeta = version().minecraft_meta;
  for (const field of fields) emptyMeta[field] = '';
  assert.equal(minecraftVersionLabel(version({ id: 'id-fallback', minecraft_meta: emptyMeta })), 'id-fallback');
  assert.equal(
    minecraftVersionLabel(version({ id: '', minecraft_meta: emptyMeta }), 'final-fallback'),
    'final-fallback',
  );
});

type LooseVNode = VNode<Record<string, any>>;

function trayButtons(tray: VNode): LooseVNode[] {
  return toChildArray(tray.props.children).filter(
    (child): child is LooseVNode => typeof child === 'object' && child !== null && child.type === Button,
  );
}

test('selection tray derives zero, one, many, select-all, clear, and actions from one required selection', () => {
  let clears = 0;
  let selects = 0;
  let actions = 0;
  const callbacks = {
    clear: () => {
      clears += 1;
    },
    selectAll: () => {
      selects += 1;
    },
  };

  assert.equal(
    SelectionActionTray({ selection: { selectedCount: 0, allSelected: false, ...callbacks }, actions: [] }),
    null,
  );

  const one = SelectionActionTray({
    selection: { selectedCount: 1, allSelected: true, ...callbacks },
    itemLabel: 'world',
    actions: [{ label: 'Delete', onClick: () => (actions += 1) }],
  });
  assert.ok(one);
  assert.equal(one.props.ariaLabel, '1 selected world');
  const oneButtons = trayButtons(one);
  assert.deepEqual(
    oneButtons.map((button) => button.props.children),
    ['Clear', 'Delete'],
  );
  (oneButtons[0].props.onClick as () => void)();
  (oneButtons[1].props.onClick as () => void)();
  assert.deepEqual({ clears, actions }, { clears: 1, actions: 1 });

  const many = SelectionActionTray({
    selection: { selectedCount: 3, allSelected: false, ...callbacks },
    itemLabel: 'world',
    actions: [],
  });
  assert.ok(many);
  assert.equal(many.props.ariaLabel, '3 selected worlds');
  const manyButtons = trayButtons(many);
  assert.deepEqual(
    manyButtons.map((button) => button.props.children),
    ['Select all', 'Clear'],
  );
  (manyButtons[0].props.onClick as () => void)();
  assert.equal(selects, 1);
});
