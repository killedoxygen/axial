import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { hashStr } from '../tokens';
import { useTheme } from '../hooks/use-theme';

// Procedural thumbnail, hash seeded gradient with soft orbs and diagonal stripes
// Same input name always produces the same tile
export function Thumb({ name, size = 64, radius, style }: {
  name: string;
  size?: number | string;
  radius?: number;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const theme = useTheme();
  const seed = useMemo(() => {
    const h = hashStr(name || 'x');
    const hue1 = h % 360;
    const hue2 = (hue1 + 40 + ((h >> 8) % 80)) % 360;
    const angle = (h >> 4) % 360;
    const stripe = ((h >> 12) % 20) + 18;
    const rot = ((h >> 2) % 90) - 45;
    const orbs = [
      { x: (h & 0xff) / 255, y: ((h >> 8) & 0xff) / 255, r: 0.35 + ((h >> 16) & 0xff) / 255 * 0.25 },
      { x: ((h >> 4) & 0xff) / 255, y: ((h >> 12) & 0xff) / 255, r: 0.28 + ((h >> 20) & 0xff) / 255 * 0.22 },
    ];
    const id = `cpt-${h.toString(36)}`;
    return { h, hue1, hue2, angle, stripe, rot, orbs, id };
  }, [name]);

  const dark = theme.dark;
  const L1 = dark ? 0.32 : 0.74;
  const L2 = dark ? 0.22 : 0.88;
  const C = 0.12;
  const r = typeof radius === 'number' ? radius : theme.r.md;
  const dim = typeof size === 'number' ? `${size}px` : size;
  const { id, hue1, hue2, angle, stripe, rot, orbs } = seed;

  return (
    <div style={{
      width: dim, height: dim, borderRadius: r,
      overflow: 'hidden', position: 'relative', flexShrink: 0,
      background: `linear-gradient(${angle}deg, oklch(${L1} ${C} ${hue1}), oklch(${L2} ${C} ${hue2}))`,
      ...style,
    }}>
      <svg width="100%" height="100%" viewBox="0 0 100 100" preserveAspectRatio="none"
        style={{ position: 'absolute', inset: 0, display: 'block' }}>
        <defs>
          <radialGradient id={`${id}-orb`} cx="50%" cy="50%" r="50%">
            <stop offset="0%" stop-color={`oklch(${dark ? 0.85 : 0.45} 0.14 ${hue2})`} stop-opacity="0.55" />
            <stop offset="100%" stop-color={`oklch(${dark ? 0.85 : 0.45} 0.14 ${hue2})`} stop-opacity="0" />
          </radialGradient>
          <pattern id={`${id}-stripe`} x="0" y="0" width={stripe} height={stripe}
            patternUnits="userSpaceOnUse" patternTransform={`rotate(${rot})`}>
            <line x1="0" y1="0" x2="0" y2={stripe} stroke={`oklch(${dark ? 1 : 0} 0 0 / 0.05)`} stroke-width="1" />
          </pattern>
        </defs>
        <rect x="0" y="0" width="100" height="100" fill={`url(#${id}-stripe)`} />
        {orbs.map((o, i) => (
          <circle key={i} cx={o.x * 100} cy={o.y * 100} r={o.r * 60} fill={`url(#${id}-orb)`} />
        ))}
        <rect x="0" y="0" width="100" height="40" fill="oklch(1 0 0 / 0.06)" />
      </svg>
    </div>
  );
}

export function initialsOf(name: string): string {
  const parts = (name || '?').trim().split(/\s+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return (parts[0] || '?').slice(0, 2).toUpperCase();
}
