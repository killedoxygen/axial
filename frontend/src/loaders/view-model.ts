import type { LoaderBuildRecord } from './types';

export function pickPreferredBuild(builds: LoaderBuildRecord[]): LoaderBuildRecord | null {
  return builds
    .slice()
    .sort((left, right) => right.build_meta.selection.default_rank - left.build_meta.selection.default_rank)[0]
    || null;
}
