import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createFrontendBuildSemantics } from './build-config.mjs';
import {
  acquireFrontendGenerationLease,
  buildAndPublishFrontendGeneration,
  cleanFrontendGenerationOwned,
  parseBuildInvocation,
  publishFrontendGeneration,
  reconcileFrontendPublication,
  watchFrontendPublicationInputs,
} from './build-generation.mjs';

const invocation = parseBuildInvocation(process.argv.slice(2));
const frontendRoot = fileURLToPath(new URL('.', import.meta.url));
const publicRoot = path.join(frontendRoot, 'static');
const outputRoot = path.join(frontendRoot, 'dist');
const portOverride = process.env.PORT;
const defaultDevPort = 3000;
const webApiBase = process.env.AXIAL_WEB_API_BASE ?? 'http://127.0.0.1:43430';
const enableDevLab = invocation.mode === 'serve';
const enableMockApi = invocation.mode === 'serve' && invocation.mock;
const semantics = createFrontendBuildSemantics({ enableDevLab, enableMockApi, webApiBase });

function buildOptions(outdir) {
  return {
    absWorkingDir: frontendRoot,
    entryPoints: { app: 'src/main.tsx' },
    bundle: true,
    outdir,
    format: 'esm',
    splitting: true,
    chunkNames: 'chunks/[name]-[hash]',
    external: ['fonts/*'],
    write: false,
    ...semantics,
  };
}

const sizeReporter = {
  name: 'size',
  setup(builder) {
    builder.onEnd((result) => {
      if (result.errors.length) return;
      for (const [outputPath, output] of Object.entries(result.metafile?.outputs ?? {})) {
        if (output.entryPoint || output.imports.some((imported) => imported.kind === 'dynamic-import')) {
          console.log(`  ${outputPath}  ${(output.bytes / 1024).toFixed(1)}kb`);
        }
      }
    });
  },
};

function reportGeneration(report) {
  const { metrics } = report;
  console.log(`published ${report.generation_id.slice(0, 12)}`);
  console.log(`  initial js       ${(metrics.initial_javascript / 1024).toFixed(1)}kb`);
  console.log(`  initial css      ${(metrics.initial_css / 1024).toFixed(1)}kb`);
  console.log(`  lazy             ${(metrics.lazy_total / 1024).toFixed(1)}kb`);
  console.log(`  public assets    ${(metrics.public_assets / 1024).toFixed(1)}kb`);
  console.log(`  packaged payload ${(metrics.packaged_payload / 1024).toFixed(1)}kb`);
  if (report.cleanup_pending) {
    console.warn('  previous generation cleanup is pending; the next publication will retry it');
  }
}

function publicationPlugin() {
  return {
    name: 'atomic-generation',
    setup(builder) {
      builder.onEnd(async (result) => {
        if (result.errors.length) return;
        try {
          reportGeneration(
            await publishFrontendGeneration({
              buildResult: result,
              frontendRoot,
              outputRoot,
              publicRoot,
            }),
          );
        } catch (error) {
          return {
            errors: [{ text: error instanceof Error ? error.message : String(error) }],
          };
        }
      });
    },
  };
}

let currentContext;
let publicationWatcher;
let shuttingDown = false;

async function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  try {
    await currentContext?.dispose();
    await publicationWatcher?.close();
  } catch {
    code = 1;
  }
  process.exit(code);
}

function ignorePipeLikeErrors(error) {
  if (error?.code === 'EPIPE' || error?.code === 'ENOENT') return;
  throw error;
}

process.stdout.on('error', ignorePipeLikeErrors);
process.stderr.on('error', ignorePipeLikeErrors);
process.stdin.on('error', ignorePipeLikeErrors);
process.on('SIGINT', () => void shutdown(0));
process.on('SIGTERM', () => void shutdown(0));
process.on('SIGHUP', () => void shutdown(0));

function isValidPort(value) {
  return Number.isInteger(value) && value > 0 && value <= 65535;
}

function canListenOn(port) {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.unref();
    server.once('error', () => resolve(false));
    server.listen(port, '0.0.0.0', () => {
      server.close(() => resolve(true));
    });
  });
}

async function resolveDevPort() {
  if (portOverride != null) {
    const port = Number(portOverride);
    if (!isValidPort(port)) throw new Error(`Invalid PORT value: ${portOverride}`);
    return port;
  }
  if (invocation.strictPort) return defaultDevPort;
  for (let port = defaultDevPort; port <= 65535; port += 1) {
    if (await canListenOn(port)) return port;
  }
  throw new Error('Could not find a free dev server port');
}

async function main() {
  if (invocation.mode === 'clean') {
    const release = await acquireFrontendGenerationLease(outputRoot);
    try {
      await cleanFrontendGenerationOwned(outputRoot, publicRoot);
    } finally {
      await release();
    }
    return;
  }
  const { build, context } = await import('esbuild');
  if (invocation.mode === 'serve') {
    const port = await resolveDevPort();
    const options = buildOptions(publicRoot);
    currentContext = await context({
      ...options,
      sourcemap: 'inline',
      metafile: true,
      plugins: [...options.plugins, sizeReporter],
    });
    const server = await currentContext.serve({ servedir: publicRoot, port });
    console.log(`dev -> http://localhost:${server.port}`);
    await new Promise(() => {});
    return;
  }
  if (invocation.mode === 'watch') {
    await reconcileFrontendPublication(outputRoot);
    const options = buildOptions(outputRoot);
    currentContext = await context({
      ...options,
      minify: true,
      metafile: true,
      plugins: [...options.plugins, publicationPlugin()],
    });
    await currentContext.watch();
    publicationWatcher = await watchFrontendPublicationInputs({
      frontendRoot,
      onChange: () => currentContext.rebuild(),
      onError: (error) => console.error(error instanceof Error ? error.message : error),
    });
    console.log('watching -> dist/generation.json');
    await new Promise(() => {});
    return;
  }

  reportGeneration(
    await buildAndPublishFrontendGeneration({
      buildFunction: build,
      buildOptions: { ...buildOptions(outputRoot), minify: true, metafile: true },
      frontendRoot,
      outputRoot,
      publicRoot,
    }),
  );
}

try {
  await main();
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
