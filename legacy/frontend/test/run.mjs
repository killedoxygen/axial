import { runFrontendTests } from './runner.mjs';

try {
  const rawSelectors = process.argv.slice(2);
  const commandLineSelectors = rawSelectors[0] === '--' ? rawSelectors.slice(1) : rawSelectors;
  const environmentSelector = process.env.AXIAL_FRONTEND_TEST;
  if (environmentSelector && commandLineSelectors.length > 0) {
    throw new Error('Specify a frontend test through either AXIAL_FRONTEND_TEST or argv, not both');
  }
  const selectors = environmentSelector ? [environmentSelector] : commandLineSelectors;
  process.exitCode = await runFrontendTests({ selectors });
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
