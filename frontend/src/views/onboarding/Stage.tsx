import type { JSX } from 'preact';
import { useRef } from 'preact/hooks';
import { DEFAULT_SKINS } from '../../default-skins';
import { LazySkinThreePreview } from '../accounts/LazySkinThreePreview';
import type { Player } from './use-identity';

const FIT = { top: 0.2, bottom: 0.08, side: 0.14 };
const OUTLINE: Player = { ...DEFAULT_SKINS[0], name: 'Player' };

/** The player's character: an outline until Axial knows who is playing, then built from their skin. */
export function Stage({ player }: { player: Player | null }): JSX.Element {
  // The last character stays on stage so it can come apart again instead of vanishing.
  const last = useRef(OUTLINE);
  if (player) last.current = player;
  const shown = last.current;

  return (
    <aside class="cp-ob-stage">
      <LazySkinThreePreview
        src={shown.src}
        capeSrc={shown.capeSrc}
        variant={shown.variant}
        name={shown.name}
        nametag={player ? player.name : null}
        built={player !== null}
        side="front"
        showOuterLayers
        fitPadding={FIT}
      />
    </aside>
  );
}
