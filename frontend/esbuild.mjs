import { context, build } from 'esbuild';

const mode = process.argv[2]; // 'serve', 'watch', or omitted (production build)

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

if (mode === 'serve') {
  // Standalone dev server, rebuilds per request and does not write to disk
  const ctx = await context({
    ...shared, sourcemap: 'inline', metafile: true, plugins: [sizeReporter],
  });
  const { port } = await ctx.serve({ servedir: 'static', port: 3000 });
  console.log(`dev → http://localhost:${port}`);

} else if (mode === 'watch') {
  // File watcher for wails dev, rebuilds to disk on source change
  const ctx = await context({
    ...shared, sourcemap: 'inline', metafile: true, plugins: [sizeReporter],
  });
  await ctx.watch();
  console.log('watching → static/app.js');

} else {
  // Production build
  const result = await build({ ...shared, minify: true, metafile: true });
  const bytes = result.metafile?.outputs['static/app.js']?.bytes ?? 0;
  console.log(`static/app.js  ${(bytes / 1024).toFixed(1)}kb`);
}
