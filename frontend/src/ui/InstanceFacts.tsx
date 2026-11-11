import type { JSX } from 'preact';
import { launchSessionActivityLabel, launchSessionIsPlaying } from '../launch-presenters';
import type { LaunchSession } from '../types-launch';
import type { EnrichedInstance } from '../types-instance';
import { Chip } from './Atoms';

/** What an instance is: its loader and Minecraft version. */
export function InstanceFacts({ inst }: { inst: Pick<EnrichedInstance, 'version_display'> }): JSX.Element {
  return (
    <>
      <Chip>{inst.version_display.loader_label}</Chip>
      <Chip mono>{inst.version_display.minecraft_label}</Chip>
    </>
  );
}

/** What a launched instance is doing right now. */
export function SessionChip({ session }: { session: LaunchSession }): JSX.Element {
  return (
    <Chip tone="accent" icon={launchSessionIsPlaying(session) ? 'play' : 'clock'}>
      {launchSessionActivityLabel(session)}
    </Chip>
  );
}
