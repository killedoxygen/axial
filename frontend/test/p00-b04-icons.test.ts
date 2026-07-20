import assert from 'node:assert/strict';
import test from 'node:test';
import type { VNode } from 'preact';

import { Icon, ICON_NAMES, isIconName, type IconName } from '../src/ui/Icons';

const EXPECTED_ICON_NAMES = [
  'activity',
  'archive',
  'arrow-left',
  'arrow-right',
  'arrow-up',
  'check',
  'check-circle',
  'chevron-left',
  'chevron-right',
  'chevron-down',
  'chevron-up',
  'circle-dashed',
  'clock',
  'compass',
  'copy',
  'cube',
  'dots',
  'download',
  'edit',
  'expand',
  'folder',
  'globe',
  'headphones',
  'home',
  'image',
  'info',
  'alert',
  'keyboard',
  'minus',
  'music',
  'music-off',
  'palette',
  'puzzle',
  'play',
  'player-skip',
  'plus',
  'rectangle',
  'refresh',
  'search',
  'settings',
  'sliders',
  'shield-check',
  'shield-person',
  'stack',
  'stop',
  'tag',
  'terminal',
  'trash',
  'user',
  'volume',
  'volume-off',
  'x',
] as const satisfies readonly IconName[];

type LooseProps = Record<string, any>;

function functionalResult(vnode: VNode): VNode<LooseProps> {
  assert.equal(typeof vnode.type, 'function');
  return (vnode.type as (props: LooseProps) => VNode<LooseProps>)(vnode.props);
}

test('icon registry exposes exactly the reviewed 52 semantic names', () => {
  assert.equal(ICON_NAMES.length, 52);
  assert.deepEqual(ICON_NAMES, EXPECTED_ICON_NAMES);
  assert.equal(new Set(ICON_NAMES).size, ICON_NAMES.length);
  for (const name of EXPECTED_ICON_NAMES) assert.equal(isIconName(name), true);
  for (const unknown of ['', 'reload', 'Sparkles', 'unknown']) assert.equal(isIconName(unknown), false);
});

test('every semantic icon supplies nonblank Lucide geometry with preserved presentation props', () => {
  for (const name of ICON_NAMES) {
    const component = Icon({
      name,
      size: 23,
      stroke: 1.75,
      color: '#17a673',
      style: { opacity: 0.8 },
    });
    assert.equal(component.props.size, 23, name);
    assert.equal(component.props.strokeWidth, 1.75, name);
    assert.equal(component.props.color, '#17a673', name);
    assert.equal(component.props['aria-hidden'], true, name);
    assert.equal(component.props.focusable, 'false', name);
    assert.deepEqual(component.props.style, { display: 'block', flexShrink: 0, opacity: 0.8 }, name);

    const lucide = functionalResult(component);
    assert.equal(typeof lucide.type, 'function', name);
    const iconNode = lucide.props.iconNode as unknown[];
    assert.ok(Array.isArray(iconNode) && iconNode.length > 0, `${name} must contain visible SVG geometry`);
  }
});

test('IconName rejects raw or obsolete icon vocabulary at compile time', () => {
  // @ts-expect-error `reload` was never a registry name; use the semantic `refresh` name.
  const invalidName: IconName = 'reload';
  assert.equal(isIconName(invalidName as string), false);
});
