import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Icon } from '../ui/Icons';
import { Music, musicStateVersion } from '../music';

// Compact music control with a live equalizer when playing
// Toggle and skip buttons, made to dock in the topbar
export function MusicWidget(): JSX.Element | null {
  // Re-render whenever the music module notifies (toggle / volume / track).
  musicStateVersion.value;
  const [, tick] = useState(0);
  useEffect(() => {
    const handler = () => tick(t => t + 1);
    const interval = setInterval(handler, 1000);
    return () => clearInterval(interval);
  }, []);

  const on = Music.enabled;
  const playing = Music.playing;

  return (
    <div
      class="cp-nodrag"
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 2,
        padding: 2,
        borderRadius: 999,
        background: 'var(--surface-2)',
        border: '1px solid var(--line)',
      }}
    >
      <button
        aria-label={on ? 'Pause music' : 'Play music'}
        title={on ? 'Music on' : 'Music off'}
        onClick={() => Music.toggle()}
        style={{
          width: 26, height: 26,
          border: 'none', background: 'transparent',
          color: on ? 'var(--accent)' : 'var(--text-mute)',
          borderRadius: 999,
          cursor: 'pointer',
          display: 'inline-flex', alignItems: 'center', justifyContent: 'center',
        }}
      >
        <Icon name={on ? 'music' : 'musicOff'} size={14} stroke={1.8} />
      </button>
      {playing && (
        <span style={{
          display: 'inline-flex', alignItems: 'flex-end', gap: 2,
          height: 16, padding: '0 6px',
        }}>
          {[0, 1, 2, 3].map(i => (
            <span key={i} style={{
              width: 2, height: '100%',
              background: 'var(--accent)',
              borderRadius: 1,
              transformOrigin: 'bottom',
              animation: `cp-eq 900ms ${i * 120}ms ease-in-out infinite`,
            }} />
          ))}
        </span>
      )}
      <button
        aria-label="Next track"
        title="Next track"
        onClick={() => Music.nextTrack()}
        style={{
          width: 26, height: 26,
          border: 'none', background: 'transparent',
          color: 'var(--text-mute)',
          borderRadius: 999,
          cursor: 'pointer',
          display: 'inline-flex', alignItems: 'center', justifyContent: 'center',
        }}
      >
        <Icon name="player-skip" size={14} stroke={1.8} />
      </button>
    </div>
  );
}
