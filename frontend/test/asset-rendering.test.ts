import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { resolve } from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { toChildArray, type VNode } from 'preact';
import * as Three from 'three';

import brandMark from '../../assets/brand-mark.json';
import { Sound } from '../src/sound';
import { InstanceGlyph, type VisualInstance } from '../src/ui/InstanceVisual';
import { Logo } from '../src/ui/Logo';
import { MicrosoftMark } from '../src/ui/MicrosoftMark';
import * as skinThree from '../src/views/accounts/three';
import { LOADER_LABELS, type LoaderKey } from '../src/views/create/defaults';
import { LoaderLogo, loaderLogoSrc } from '../src/views/create/loader-logos';

type LooseProps = Record<string, any>;

function functionalResult(vnode: VNode): VNode<LooseProps> {
  assert.equal(typeof vnode.type, 'function');
  return (vnode.type as (props: LooseProps) => VNode<LooseProps>)(vnode.props);
}

test('the skin renderer receives the original Three exports through its narrow boundary', () => {
  assert.equal(Object.keys(skinThree).length, 18);
  for (const [name, value] of Object.entries(skinThree)) {
    assert.equal(value, Three[name as keyof typeof Three], name);
  }
});

test('concurrent skin renderer loads share one promise and a failed load permits retry', async () => {
  const filename = resolve('src/views/accounts/skin-three-loader.ts');
  const ts: typeof import('typescript') = createRequire(resolve('package.json'))('typescript');
  const output = ts.transpileModule(await readFile(filename, 'utf8'), {
    fileName: filename,
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  });
  const exports = {} as typeof import('../src/views/accounts/skin-three-loader');
  const failure = new Error('Skin renderer chunk unavailable');
  let attempts = 0;
  vm.runInNewContext(
    output.outputText,
    {
      exports,
      require(id: string) {
        assert.equal(id, './three');
        if (++attempts === 1) throw failure;
        return skinThree;
      },
    },
    { filename },
  );
  const first = exports.loadThree();
  assert.equal(exports.loadThree(), first);
  await assert.rejects(first, (error: unknown) => error === failure);
  assert.equal(attempts, 1);

  const retry = exports.loadThree();
  assert.notEqual(retry, first);
  assert.equal(exports.loadThree(), retry);
  assert.equal(await retry, skinThree);
  assert.equal(exports.loadThree(), retry);
  assert.equal(attempts, 2);
});

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

test('all LoaderKey values retain their original assets and alignment in create and instance glyphs', async () => {
  const expected: Record<LoaderKey, string> = {
    vanilla: 'vanilla_icon.svg',
    fabric: 'fabric_icon.svg',
    forge: 'forge_icon.svg',
    neoforge: 'neoforge_icon.svg',
    quilt: 'quilt_icon.svg',
  };
  const originalHashes: Record<LoaderKey, string> = {
    vanilla: '34abce831779c6bc43e91214e7e57c8ed8c4c3e0e63b58e38d6e7f9132585b28',
    fabric: '724a6ace8aab39ffeca373e847782c5ff56abc0bc898f962519bd9aa0b696f8e',
    forge: '48b1a0bd969b3f96dc9a154b1041c954aa6b76334873b6d332fffc47b4e7d908',
    neoforge: 'cdccab15e2e71a10ad21d1e4cd766c29fa10dbb48768266e713b0c4b694a5dac',
    quilt: 'bee418e2d25ef96bff9a5fd74851548098744580028042aaaf29baadfbef1fc5',
  };
  const loaderKeys = Object.keys(LOADER_LABELS) as LoaderKey[];
  assert.deepEqual(Object.fromEntries(loaderKeys.map((loader) => [loader, loaderLogoSrc(loader)])), expected);
  assert.equal(new Set(Object.values(expected)).size, loaderKeys.length);

  for (const loader of loaderKeys) {
    const createMark = LoaderLogo({ loader, size: 18 });
    assert.equal(createMark.type, 'span');
    assert.equal(createMark.props['data-loader'], loader);
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
    assert.equal((glyph.props as LooseProps)['data-loader'], loader);
    assert.equal((glyph.props as LooseProps).class, 'fixture-glyph fixture-glyph--mask');
    assert.equal((glyph.props as LooseProps).style['--cp-loader-src'], `url("${expected[loader]}")`);
    assert.equal(
      createHash('sha256')
        .update(await readFile(`static/${expected[loader]}`))
        .digest('hex'),
      originalHashes[loader],
    );
  }

  const createStyle = await readFile('src/views/create/create.css', 'utf8');
  assert.match(createStyle, /\.cp-cr-loader-mark\[data-loader='quilt'\]\s*\{\s*translate: 2\.1% 2\.1%;\s*\}/);
  assert.match(createStyle, /\.cp-cr-loader-mark\[data-loader='forge'\]\s*\{\s*translate: 0 2\.1%;\s*\}/);
  const tileStyle = await readFile('src/ui/instance-visual.css', 'utf8');
  assert.match(tileStyle, /\.cp-tile-glyph--mask\[data-loader='quilt'\]\s*\{\s*translate: 2\.1cqmin 2\.1cqmin;\s*\}/);
  assert.match(tileStyle, /\.cp-tile-glyph--mask\[data-loader='forge'\]\s*\{\s*translate: 0 2\.1cqmin;\s*\}/);
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
