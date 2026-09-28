import type { BuildOptions, BuildResult, Metafile, OutputFile } from 'esbuild';

export type BuildMode = 'build' | 'serve' | 'watch' | 'clean';

export type BundleMetricKey =
  | 'initial_javascript'
  | 'initial_css'
  | 'lazy_total'
  | 'public_assets'
  | 'largest_public_asset'
  | 'largest_generated_output'
  | 'generated_total'
  | 'packaged_payload';

export type BundleMetrics = Record<BundleMetricKey, number>;

export interface GenerationFile {
  path: string;
  bytes: number;
  sha256: string;
}

export interface GenerationGraphImport {
  path: string;
  dynamic: boolean;
}

export interface GenerationGraphOutput {
  path: string;
  css_bundle: string | null;
  imports: GenerationGraphImport[];
}

export interface GenerationManifest {
  schema_version: 1;
  generation_id: string;
  document_entry: 'index.html';
  script_entry: 'app.js';
  files: GenerationFile[];
  graph: GenerationGraphOutput[];
}

export type GenerationIdentity = Omit<GenerationManifest, 'generation_id'>;

export interface VerifiedGeneration extends GenerationManifest {
  metrics: BundleMetrics;
}

export interface GenerationReport {
  generation_id: string;
  metrics: BundleMetrics;
  cleanup_pending: boolean;
}

export interface PublishedBuildResult {
  metafile: Metafile;
  outputFiles: OutputFile[];
}

export interface PublicationOptions {
  buildResult: PublishedBuildResult;
  frontendRoot: string;
  outputRoot: string;
  publicRoot?: string;
  publicManifestPath?: string;
  budgetPath?: string;
  scriptEntryPoint?: string;
}

export interface BuildAndPublicationOptions extends Omit<PublicationOptions, 'buildResult'> {
  buildFunction: (options: BuildOptions) => Promise<BuildResult>;
  buildOptions: BuildOptions;
}

export function parseBuildInvocation(argv: string[]): Readonly<{
  mode: BuildMode;
  strictPort: boolean;
  mock: boolean;
}>;

export function parsePublicAssetManifest(source: string): readonly string[];

export function parseBundleBudgets(source: string): Readonly<BundleMetrics>;

export function enforceBundleBudgets(metrics: BundleMetrics, maximumBytes: BundleMetrics): void;

export function measureBundle(options: {
  metafile: Metafile;
  outputFiles: OutputFile[];
  outputRoot: string;
  workingDirectory: string;
  publicFiles: Array<{ path: string; bytes: Uint8Array }>;
  scriptEntryPoint: string;
}): {
  generated: Map<string, Buffer>;
  graph: GenerationGraphOutput[];
};

export function deriveBundleMetrics(options: {
  files: Array<{ path: string; bytes: number }>;
  publicPaths: Iterable<string>;
  graph: GenerationGraphOutput[];
  scriptEntry: string;
  receiptBytes: number;
}): Readonly<BundleMetrics>;

export function computeFrontendGenerationId(manifest: GenerationIdentity | GenerationManifest): string;

/** The caller must hold the output root's frontend generation lease. */
export function replacePublishedDirectoryOwned(
  stage: string,
  outputRoot: string,
  operations?: {
    renamePath?: (source: string, destination: string) => Promise<void>;
    removePath?: (target: string, options: { recursive: true }) => Promise<void>;
  },
): Promise<Readonly<{ cleanup_pending: boolean }>>;

export function acquireFrontendGenerationLease(outputRoot: string): Promise<() => Promise<void>>;

export function frontendGenerationLeasePort(outputRoot: string): Promise<number>;

/** The caller must hold the output root's frontend generation lease. */
export function reconcileFrontendPublicationOwned(outputRoot: string): Promise<void>;

export function reconcileFrontendPublication(outputRoot: string): Promise<void>;

export function buildAndPublishFrontendGeneration(
  options: BuildAndPublicationOptions,
): Promise<Readonly<GenerationReport>>;

export function publishFrontendGeneration(options: PublicationOptions): Promise<Readonly<GenerationReport>>;

export function verifyFrontendGeneration(
  outputRoot: string,
  authorities?: {
    publicManifestPath?: string;
    budgetPath?: string;
  },
): Promise<Readonly<VerifiedGeneration>>;

export function watchFrontendPublicationInputs(options: {
  frontendRoot: string;
  onChange: () => Promise<unknown>;
  onError: (error: unknown) => void;
}): Promise<Readonly<{ close: () => Promise<void> }>>;

/** The caller must hold the output root's frontend generation lease. */
export function cleanFrontendGenerationOwned(outputRoot: string, publicRoot?: string): Promise<void>;
