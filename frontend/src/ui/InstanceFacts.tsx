import type { JSX } from 'preact';
import { launchSessionActivityLabel, launchSessionIsPlaying } from '../launch/presenters';
import type { LaunchSession } from '../launch/state';
import type { EnrichedInstance } from '../generated/EnrichedInstance';
import { Chip } from './Atoms';

/** What an instance is: its loader and Minecraft version. */
export function InstanceFacts({ inst }: { inst: Pick<EnrichedInstance, 'version_display'> }): JSX.Element {
  return (
    <>
      <Chip>{inst.version_display.loader_label}</Chip>
      <Chip>{inst.version_display.minecraft_label}</Chip>
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
