import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { basename, resolve } from 'node:path';
import test from 'node:test';
import { api, isApiError } from '../../src/api';

const repositoryRoot = basename(process.cwd()) === 'frontend' ? resolve(process.cwd(), '..') : process.cwd();
/** @param {string} path */
const read = (path) => readFile(resolve(repositoryRoot, path), 'utf8');

/** @param {string} directory @returns {Promise<string[]>} */
async function rustFiles(directory) {
  const entries = await readdir(resolve(repositoryRoot, directory), { withFileTypes: true });
  const nested = await Promise.all(
    entries.map((entry) =>
      entry.isDirectory()
        ? rustFiles(`${directory}/${entry.name}`)
        : Promise.resolve(entry.name.endsWith('.rs') ? [`${directory}/${entry.name}`] : []),
    ),
  );
  return nested.flat();
}

test('every current JSON and query handler captures rejection before returning a public response', async () => {
  const paths = [...(await rustFiles('apps/api/src/routes')), 'apps/api/src/transport.rs'];
  let jsonInputs = 0;
  let queryInputs = 0;
  for (const path of paths) {
    const source = (await read(path)).split(/\n#\[cfg\(test\)\]\s*\nmod tests\b/)[0] ?? '';
    // Feature-owned Result extractors replaced the legacy ApiJson/ApiQuery
    // wrapper. Bare extractors would restore Axum's raw text response.
    assert.equal(
      /\b(?:Json|Query)\s*\([^)]*\)\s*:\s*(?:Json|Query)</.test(source),
      false,
      `${path}: bare input extractor`,
    );
    assert.equal(
      /\b(?:request|body|query|parameters)\s*:\s*(?:Json|Query)</.test(source),
      false,
      `${path}: bare input extractor`,
    );
    const json = source.match(/Result<(?:Option<)?Json</g)?.length ?? 0;
    const query = source.match(/Result<Query</g)?.length ?? 0;
    if (source.includes('JsonRejection'))
      assert.match(source, /DefaultBodyLimit::max\(/, `${path}: explicit JSON limit`);
    assert.doesNotMatch(source, /(?:rejection|error)\.body_text\(\)/, path);
    assert.doesNotMatch(source, /rejection\.to_string\(\)|format!\([^\n]*rejection/, path);
    jsonInputs += json;
    queryInputs += query;
  }
  assert.ok(jsonInputs > 0 && queryInputs > 0, 'the current input inventory must not be empty');
  const skin = await read('apps/api/src/routes/skin.rs');
  assert.match(skin, /to_bytes\(body, 4096\)[\s\S]*?map_err\(\|_\| invalid_request\(\)\)/);
  assert.match(skin, /serde_json::from_slice\(&bytes\)\.map_err\(\|_\| invalid_request\(\)\)/);
});

test('settings rejection mapping uses a fixed JSON vocabulary rather than extractor details', async () => {
  const config = await read('apps/api/src/routes/config.rs');
  const mapper = config.match(/fn json_error\([\s\S]*?\n\}/)?.[0] ?? '';
  assert.ok(mapper);
  for (const message of [
    'Invalid JSON request.',
    'Invalid JSON syntax.',
    'JSON content type is required.',
    'Request body is too large.',
  ])
    assert.ok(mapper.includes(JSON.stringify(message)), message);
  assert.match(mapper, /StatusCode::PAYLOAD_TOO_LARGE/);
  assert.match(mapper, /StatusCode::UNSUPPORTED_MEDIA_TYPE/);
  assert.match(mapper, /Json\(json!\(\{ "error": message \}\)\)/);
  assert.doesNotMatch(mapper, /\.body_text\(\)|\.to_string\(\)|format!/);
});

test('client rejects HTTP errors without exposing raw rejection bodies and bounds public messages', async () => {
  const originalFetch = globalThis.fetch;
  try {
    for (const body of ['private SQL at /private/profile', '<html>private proxy detail</html>', '{bad-private-json']) {
      globalThis.fetch = async () => new Response(body, { status: 400, headers: { 'content-type': 'text/plain' } });
      await assert.rejects(api('POST', '/config', {}), (error) => {
        assert.ok(isApiError(error));
        assert.equal(error.status, 400);
        assert.equal(error.payload, undefined);
        assert.equal(error.message, 'Request failed with HTTP 400');
        return true;
      });
    }
    globalThis.fetch = async () =>
      new Response(JSON.stringify({ error: `  ${'bounded '.repeat(100)}  ` }), {
        status: 422,
        headers: { 'content-type': 'application/json' },
      });
    await assert.rejects(api('PUT', '/config', {}), (error) => {
      assert.ok(isApiError(error));
      assert.equal(error.status, 422);
      assert.equal(error.message.length, 180);
      assert.ok(error.message.endsWith('...'));
      return true;
    });
  } finally {
    globalThis.fetch = originalFetch;
  }
});
