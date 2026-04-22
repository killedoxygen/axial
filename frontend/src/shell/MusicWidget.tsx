import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { Music, musicStateVersion } from '../music';

// Compact music control
// When off, only the toggle is visible
// When on, show equalizer during playback plus a skip button
export function MusicWidget(): JSX.Element | null {
  musicStateVersion.value;
  const [, tick] = useState(0);
  useEffect(() => {
    const handler = (): void => tick(t => t + 1);
    const interval = setInterval(handler, 1000);
    return () => clearInterval(interval);
  }, []);

  const on = Music.enabled;
  const playing = Music.playing;

  return (
    <div class={`cp-music-widget cp-nodrag${on ? ' cp-music-widget--on' : ''}`}>
      <button
        class="cp-music-btn"
        data-active={on}
        aria-label={on ? 'Pause music' : 'Play music'}
        title={on ? (playing ? 'Playing ambient music' : 'Music on') : 'Music off'}
        onClick={() => Music.toggle()}
      >
        {on && playing ? (
          <span class="cp-music-eq" aria-hidden="true">
            <span style={{ animationDelay: '0ms' }} />
            <span style={{ animationDelay: '120ms' }} />
            <span style={{ animationDelay: '240ms' }} />
            <span style={{ animationDelay: '360ms' }} />
          </span>
        ) : (
          <Icon name={on ? 'music' : 'music-off'} size={14} stroke={1.8} />
        )}
      </button>
      {on && (
        <button
          class="cp-music-btn cp-music-btn--skip"
          aria-label="Next track"
          title="Next track"
          onClick={() => Music.nextTrack()}
        >
          <Icon name="player-skip" size={13} stroke={1.8} />
        </button>
      )}
    </div>
  );
}
