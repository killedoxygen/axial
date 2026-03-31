import { build, context } from 'esbuild';

const isDev = process.argv.includes('--dev');
const isWatch = process.argv.includes('--watch');

const options = {
  entryPoints: ['src/main.tsx'],
  bundle: true,
  outfile: 'static/app.js',
  format: 'iife',
  target: ['es2020'],
  minify: !isDev,
  sourcemap: isDev ? 'inline' : false,
  logLevel: 'info',
  jsx: 'automatic',
  jsxImportSource: 'preact',
};

if (isWatch) {
  const ctx = await context(options);
  await ctx.watch();
  console.log('esbuild watching frontend/static/app.js');
  await new Promise(() => {});
} else {
  await build(options);
}
