import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

const requestRouteFiles = [
  'accounts.rs',
  'config.rs',
  'content.rs',
  'flags.rs',
  'install.rs',
  'instances.rs',
  'launch/mod.rs',
  'loaders.rs',
  'music.rs',
  'performance.rs',
  'skin.rs',
  'telemetry.rs',
  'update.rs',
  'version_info.rs',
];

test('all JSON and query inputs use the bounded API extractors', async () => {
  const routes = (await Promise.all(requestRouteFiles.map((path) => read(`apps/api/src/routes/${path}`)))).join('\n');

  assert.equal(routes.match(/ApiJson\([^)]*\): ApiJson</g)?.length, 31);
  assert.equal(routes.match(/ApiQuery\([^)]*\): ApiQuery</g)?.length, 26);
  assert.equal(routes.match(/Option<ApiJson</g)?.length, 1);
  assert.doesNotMatch(routes, /\bJson\([^)]*\): Json</);
  assert.doesNotMatch(routes, /\bQuery\([^)]*\): Query</);
  assert.doesNotMatch(routes, /JsonRejection|QueryRejection/);
});

test('extractor rejection vocabulary is fixed JSON without raw rejection text', async () => {
  const [extract, frontend] = await Promise.all([read('apps/api/src/routes/extract.rs'), read('frontend/src/api.ts')]);

  for (const message of [
    'Invalid JSON request.',
    'Invalid JSON syntax.',
    'JSON content type is required.',
    'JSON request is too large.',
    'Invalid query request.',
  ]) {
    assert.match(
      extract,
      new RegExp(
        JSON.stringify(message)
          .slice(1, -1)
          .replace(/[.*+?^${}()|[\]\\]/g, '\\$&'),
      ),
    );
  }
  assert.match(extract, /JsonRejection::JsonSyntaxError/);
  assert.match(extract, /JsonRejection::MissingJsonContentType/);
  assert.match(extract, /StatusCode::PAYLOAD_TOO_LARGE/);
  assert.match(extract, /Json\(serde_json::json!\(\{ "error": self\.error \}\)\)/);
  assert.doesNotMatch(extract, /rejection\.body_text\(\)|rejection\.to_string\(\)|format!\([^\n]*rejection/);
  assert.match(frontend, /if \(!response\.ok && !looksJson\(response, text\)\) return undefined/);
});
