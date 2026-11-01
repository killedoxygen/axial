import assert from 'node:assert/strict';
import test from 'node:test';

import {
  compareContentRows,
  contentRowMatches,
  fileDisplayName,
  type ContentRowView,
} from '../src/views/instance/components/content-row';
import { packMembership } from '../src/views/instance/components/pack-origin';

function row(title: string, fileName: string, overrides: Partial<ContentRowView> = {}): ContentRowView {
  return {
    key: fileName,
    title,
    fileName,
    details: [],
    fallbackIcon: 'puzzle',
    managed: false,
    enabled: true,
    ...overrides,
  };
}

test('installed content is ordered by display title regardless of case, numbering or filename', () => {
  const rows = [
    row('sodium', 'aaa.jar'),
    row('Mod 10', 'b.jar'),
    row('fabric api', 'z-fabric.jar'),
    row('AppleSkin', 'zzz.jar'),
    row('Mod 2', 'c.jar'),
    row('Fabric API', 'a-fabric.jar'),
  ];
  assert.deepEqual(
    rows.sort(compareContentRows).map((view) => view.fileName),
    ['zzz.jar', 'a-fabric.jar', 'z-fabric.jar', 'c.jar', 'b.jar', 'aaa.jar'],
  );
});

test('search matches titles, filenames, details and pack names, and files fall back to their base name', () => {
  const view = row('Sodium', 'sodium-fabric-0.7.2.jar', {
    details: ['0.7.2', 'JellySquid, IMS'],
    pack: 'Fabulously Optimized',
  });
  for (const query of ['', 'SODIUM', 'fabric-0.7', 'jellysquid', 'fabulously']) {
    assert.equal(contentRowMatches(view, query), true, query);
  }
  assert.equal(contentRowMatches(view, 'lithium'), false);
  assert.equal(fileDisplayName('handmade-patch.jar.disabled'), 'handmade-patch');
  assert.equal(fileDisplayName('Faithful 32x.ZIP'), 'Faithful 32x');
  assert.equal(fileDisplayName('Unpacked tweaks'), 'Unpacked tweaks');
});

test('pack membership maps recorded instance paths to the pack that placed them', () => {
  const members = packMembership([
    {
      canonical_id: 'modrinth:fo',
      project_id: 'fo',
      title: 'Fabulously Optimized',
      version_id: 'v',
      paths: ['mods/sodium.jar'],
    },
    { canonical_id: 'modrinth:x', project_id: 'x', version_id: 'v', paths: ['resourcepacks/pack.zip'] },
  ]);
  assert.equal(members.get('mods/sodium.jar'), 'Fabulously Optimized');
  assert.equal(members.get('resourcepacks/pack.zip'), 'modpack');
  assert.equal(members.get('mods/other.jar'), undefined);
});
