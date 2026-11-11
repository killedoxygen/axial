import assert from 'node:assert/strict';
import test from 'node:test';
import type { VNode } from 'preact';

import { Icon, ICON_NAMES, isIconName, type IconName } from '../src/ui/Icons';

type LooseProps = Record<string, any>;

function functionalResult(vnode: VNode): VNode<LooseProps> {
  assert.equal(typeof vnode.type, 'function');
  return (vnode.type as (props: LooseProps) => VNode<LooseProps>)(vnode.props);
}

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
