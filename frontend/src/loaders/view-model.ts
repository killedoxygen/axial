import type { LoaderBuildRecord } from './types';

export function pickPreferredBuild(builds: LoaderBuildRecord[]): LoaderBuildRecord | null {
  return builds.find((build) => build.recommended)
    || builds.find((build) => build.latest)
    || builds.find((build) => build.stable)
    || builds[0]
    || null;
}
