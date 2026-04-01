import net from 'node:net';
import { context, build } from 'esbuild';

const mode = process.argv[2]; // 'serve', 'watch', or omitted (production build)
const portOverride = process.env.PORT;
const defaultDevPort = 3000;

const shared = {
  entryPoints: { app: 'src/main.tsx' },
  bundle: true,
  outdir: 'static',
  format: 'iife',
  target: ['es2020'],
  jsx: 'automatic',
  jsxImportSource: 'preact',
};

const sizeReporter = {
  name: 'size',
  setup(b) {
    b.onEnd(result => {
      if (result.errors.length) return;
      const out = result.metafile?.outputs['static/app.js'];
      if (out) console.log(`  static/app.js  ${(out.bytes / 1024).toFixed(1)}kb`);
    });
  },
};

let currentCtx;
let shuttingDown = false;

async function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  try {
    await currentCtx?.dispose();
  } catch {}
  process.exit(code);
}

function ignorePipeLikeErrors(err) {
  if (err?.code === 'EPIPE' || err?.code === 'ENOENT') return;
  throw err;
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
  return new Promise(resolve => {
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
    if (!isValidPort(port)) {
      throw new Error(`Invalid PORT value: ${portOverride}`);
    }
    return port;
  }

  for (let port = defaultDevPort; port <= 65535; port += 1) {
    if (await canListenOn(port)) return port;
  }

  throw new Error('Could not find a free dev server port');
}

if (mode === 'serve') {
  // Standalone dev server, rebuilds per request and does not write to disk
  const port = await resolveDevPort();
  currentCtx = await context({
    ...shared, sourcemap: 'inline', metafile: true, plugins: [sizeReporter],
  });
  const server = await currentCtx.serve({ servedir: 'static', port });
  console.log(`dev → http://localhost:${server.port}`);
  await new Promise(() => {});
} else if (mode === 'watch') {
  // File watcher for wails dev, rebuilds to disk on source change
  currentCtx = await context({
    ...shared, sourcemap: 'inline', metafile: true, plugins: [sizeReporter],
  });
  await currentCtx.watch();
  console.log('watching → static/app.js');
  await new Promise(() => {});
} else {
  // Production build
  const result = await build({ ...shared, minify: true, metafile: true });
  const bytes = result.metafile?.outputs['static/app.js']?.bytes ?? 0;
  console.log(`static/app.js  ${(bytes / 1024).toFixed(1)}kb`);
}
