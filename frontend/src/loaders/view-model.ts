import type { LoaderBuildRecord } from './types';

export function pickPreferredBuild(builds: LoaderBuildRecord[]): LoaderBuildRecord | null {
  return builds.find((build) => build.recommended)
    || builds.find((build) => build.stable)
    || builds.find((build) => build.latest)
    || builds[0]
    || null;
}
