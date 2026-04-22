import type { JSX } from 'preact';
import { useEffect, useRef } from 'preact/hooks';
import { initColorField, positionFieldMarker } from '../../theme';

// Hue by chroma picker, x is hue 0..360, y is chroma with vivid at top
export function ColorField({
  hue, vibrancy, onChange, onEnd,
}: {
  hue: number;
  vibrancy: number;
  onChange: (hue: number, vibrancy: number) => void;
  onEnd?: () => void;
}): JSX.Element {
  const fieldRef = useRef<HTMLDivElement>(null);
  const markerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    initColorField(fieldRef.current, markerRef.current, onChange, onEnd);
    positionFieldMarker(fieldRef.current, markerRef.current, hue, vibrancy);
    // Initial binding only, handler refs close over stable refs
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    positionFieldMarker(fieldRef.current, markerRef.current, hue, vibrancy);
  }, [hue, vibrancy]);

  return (
    <div
      ref={fieldRef}
      style={{
        position: 'relative',
        width: '100%',
        height: 140,
        borderRadius: 'var(--r-md)',
        cursor: 'crosshair',
        background:
          'linear-gradient(to bottom, transparent 0%, var(--surface) 100%), ' +
          'linear-gradient(to right, oklch(0.78 0.14 0), oklch(0.78 0.14 60), oklch(0.78 0.14 120), oklch(0.78 0.14 180), oklch(0.78 0.14 240), oklch(0.78 0.14 300), oklch(0.78 0.14 360))',
        border: '1px solid var(--line)',
        overflow: 'hidden',
        touchAction: 'none',
      }}
    >
      <div
        ref={markerRef}
        style={{
          position: 'absolute',
          width: 16,
          height: 16,
          borderRadius: '50%',
          border: '2px solid white',
          boxShadow: '0 1px 6px rgba(0,0,0,0.5)',
          transform: 'translate(-50%, -50%)',
          pointerEvents: 'none',
          transition: 'left 80ms ease, top 80ms ease, background 80ms ease',
        }}
      />
    </div>
  );
}
