import { fileURLToPath } from 'node:url';

/** @typedef {{ dependencyRoot?: string }} ResolverOptions */
/**
 * @typedef {object} BuildSemanticsOptions
 * @property {string} [dependencyRoot]
 * @property {boolean} enableDevLab
 * @property {boolean} enableMockApi
 * @property {string} webApiBase
 */

const defaultDependencyRoot = fileURLToPath(new URL('.', import.meta.url));
const reactCompatAliases = new Map([
  ['react', 'preact/compat'],
  ['react-dom', 'preact/compat'],
  ['react/jsx-runtime', 'preact/jsx-runtime'],
  ['react/jsx-dev-runtime', 'preact/jsx-runtime'],
]);

/** @param {ResolverOptions} [options] @returns {import('esbuild').Plugin[]} */
export function createFrontendResolverPlugins({ dependencyRoot = defaultDependencyRoot } = {}) {
  return [
    {
      name: 'preact-compat-alias',
      /** @param {import('esbuild').PluginBuild} build */
      setup(build) {
        build.onResolve({ filter: /^react(?:-dom|\/jsx-runtime|\/jsx-dev-runtime)?$/ }, async (args) => {
          const target = reactCompatAliases.get(args.path);
          if (!target) return;
          return build.resolve(target, { kind: args.kind, resolveDir: dependencyRoot });
        });
      },
    },
  ];
}

/**
 * @param {BuildSemanticsOptions} options
 * @returns {Pick<import('esbuild').BuildOptions, 'define' | 'jsx' | 'jsxImportSource' | 'plugins' | 'target'>}
 */
export function createFrontendBuildSemantics({ dependencyRoot, enableDevLab, enableMockApi, webApiBase }) {
  return {
    target: ['es2020'],
    jsx: 'automatic',
    jsxImportSource: 'preact',
    define: {
      __AXIAL_WEB_API_BASE__: JSON.stringify(webApiBase),
      __AXIAL_ENABLE_DEV_LAB__: JSON.stringify(enableDevLab),
      __AXIAL_MOCK_API__: JSON.stringify(enableMockApi),
    },
    plugins: createFrontendResolverPlugins({ dependencyRoot }),
  };
}
