import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import { toChildArray, type VNode } from 'preact';

import brandMark from '../../assets/brand-mark.json';
import { Sound } from '../src/sound';
import { InstanceGlyph, type VisualInstance } from '../src/ui/InstanceVisual';
import { Logo } from '../src/ui/Logo';
import { MicrosoftMark } from '../src/ui/MicrosoftMark';
import { LOADER_KEYS, type LoaderKey } from '../src/views/create/defaults';
import { LoaderLogo, loaderLogoSrc } from '../src/views/create/loader-logos';

type LooseProps = Record<string, any>;

function functionalResult(vnode: VNode): VNode<LooseProps> {
  assert.equal(typeof vnode.type, 'function');
  return (vnode.type as (props: LooseProps) => VNode<LooseProps>)(vnode.props);
}

test('Logo projects every path and viewBox directly from the sole brand manifest', () => {
  const logo = Logo({ size: 40 });
  assert.equal(logo.type, 'svg');
  assert.equal(logo.props.viewBox, brandMark.view_box.join(' '));
  assert.equal(logo.props.width, 40);
  assert.equal(logo.props.height, 40);

  const paths = toChildArray(logo.props.children).map((group) => {
    assert.equal(typeof group, 'object');
    return toChildArray((group as VNode).props.children)[0] as VNode;
  });
  assert.deepEqual(
    paths.map((path) => (path.props as LooseProps).d),
    [brandMark.paths.ribbon, brandMark.paths.top_right, brandMark.paths.bottom_left],
  );
  assert.ok(paths.every((path) => (path.props as LooseProps).fill === brandMark.colors.interface));
  assert.equal((paths[0].props as LooseProps).fillRule, 'evenodd');
});

test('all LoaderKey values produce distinct neutral assets in create and instance glyphs', () => {
  const expected: Record<LoaderKey, string> = {
    vanilla: 'loader-base.svg',
    fabric: 'loader-grid.svg',
    forge: 'loader-cross.svg',
    neoforge: 'loader-orbit.svg',
    quilt: 'loader-diamonds.svg',
  };
  assert.deepEqual(Object.fromEntries(LOADER_KEYS.map((loader) => [loader, loaderLogoSrc(loader)])), expected);
  assert.equal(new Set(Object.values(expected)).size, LOADER_KEYS.length);

  for (const loader of LOADER_KEYS) {
    const createMark = LoaderLogo({ loader, size: 18 });
    assert.equal(createMark.type, 'span');
    assert.equal(createMark.props.style['--cp-loader-src'], `url("${expected[loader]}")`);
    assert.equal(createMark.props.style.width, '18px');

    const instance: VisualInstance = {
      id: `instance-${loader}`,
      name: loader,
      version_id: 'missing-version',
      art_seed: 1,
      version_display: { loader_key: loader },
    };
    const glyph = functionalResult(InstanceGlyph({ inst: instance, className: 'fixture-glyph' }));
    assert.equal(glyph.type, 'span');
    assert.equal((glyph.props as LooseProps).class, 'fixture-glyph fixture-glyph--mask');
    assert.equal((glyph.props as LooseProps).style['--cp-loader-src'], `url("${expected[loader]}")`);
  }
});

test('Microsoft authentication uses the exact local official symbol geometry', async () => {
  const mark = MicrosoftMark({ size: 21, class: 'auth-mark' });
  assert.equal(mark.type, 'img');
  assert.equal(mark.props.src, 'microsoft-auth-symbol.svg');
  assert.equal(mark.props.width, 21);
  assert.equal(mark.props.height, 21);
  assert.equal(mark.props.alt, '');

  const source = await readFile('static/microsoft-auth-symbol.svg', 'utf8');
  assert.equal(
    source,
    '<svg xmlns="http://www.w3.org/2000/svg" width="21" height="21" viewBox="0 0 21 21"><title>MS-SymbolLockup</title><rect x="1" y="1" width="9" height="9" fill="#f25022"/><rect x="1" y="11" width="9" height="9" fill="#00a4ef"/><rect x="11" y="1" width="9" height="9" fill="#7fba00"/><rect x="11" y="11" width="9" height="9" fill="#ffb900"/></svg>',
  );
});

test('launch success prefers the retained celebration sprite and falls back to its oscillator sequence', () => {
  const originalEnabled = Sound.enabled;
  const originalActivate = Sound.activate;
  const originalPlaySprite = Sound.playSprite;
  const originalSequence = Sound.sequence;
  const spriteCalls: string[] = [];
  const sequences: unknown[][] = [];
  try {
    Sound.enabled = true;
    Sound.activate = () => {};
    Sound.playSprite = (name) => {
      spriteCalls.push(name);
      return true;
    };
    Sound.sequence = (notes) => sequences.push(notes);
    Sound.ui('launchSuccess');
    assert.deepEqual(spriteCalls, ['celebration']);
    assert.deepEqual([...sequences], []);

    Sound.playSprite = (name) => {
      spriteCalls.push(name);
      return false;
    };
    Sound.ui('launchSuccess');
    assert.deepEqual(spriteCalls, ['celebration', 'celebration']);
    const fallbackNotes = sequences[0] as unknown[] | undefined;
    assert.ok(fallbackNotes);
    assert.ok(fallbackNotes.length >= 4);
  } finally {
    Sound.enabled = originalEnabled;
    Sound.activate = originalActivate;
    Sound.playSprite = originalPlaySprite;
    Sound.sequence = originalSequence;
  }
});
