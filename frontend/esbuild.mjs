import { build } from 'esbuild';

const isDev = process.argv.includes('--dev');

await build({
  entryPoints: ['src/main.js'],
  bundle: true,
  outfile: 'static/app.js',
  format: 'iife',
  target: ['es2020'],
  minify: !isDev,
  sourcemap: isDev ? 'inline' : false,
  logLevel: 'info',
});
