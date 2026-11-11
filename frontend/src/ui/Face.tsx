import type { JSX } from 'preact';
import { useId } from 'preact/hooks';

import brandMark from '../../../assets/brand-mark.json';

/** How something ended: awake (ready, running), asleep (nothing here, waiting), wary (check this), failed, done. */
export type Eyes = 'open' | 'closed' | 'wide' | 'x' | 'happy';
export type FaceTone = 'accent' | 'warn' | 'err' | 'mute';

const HEAD = `${brandMark.paths.face.split('Z')[0]}Z`;
const GILLS = `${brandMark.paths.left_gills}${brandMark.paths.right_gills}`;

const cut = { fill: 'none', stroke: '#000', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' } as const;

/** The brand mark's features, cut out of its head. */
function features(eyes: Eyes): JSX.Element {
  const eye = (cx: number): JSX.Element => {
    if (eyes === 'closed') return <rect x={cx - 20} y={257} width={40} height={14} rx={7} />;
    if (eyes === 'wide' && cx > 250) return <rect x={cx - 16} y={226} width={32} height={54} rx={10} />;
    if (eyes === 'x') {
      return (
        <>
          {[45, -45].map((angle) => (
            <rect
              x={cx - 19.84}
              y={258.5}
              width={39.68}
              height={11}
              rx={5.5}
              transform={`rotate(${angle} ${cx} 264)`}
            />
          ))}
        </>
      );
    }
    if (eyes === 'happy')
      return <path d={`M${cx - 16} 272.96L${cx} 254.4L${cx + 16} 272.96`} {...cut} stroke-width={11} />;
    return <rect x={cx - 16} y={248} width={32} height={32} rx={10} />;
  };
  const mouth =
    eyes === 'x' ? (
      <path d="M222 304A40 40 0 0 1 278 304" {...cut} stroke-width={12} />
    ) : eyes === 'closed' ? (
      <rect x={232} y={296} width={36} height={14} rx={7} />
    ) : eyes === 'wide' ? (
      <rect x={230} y={296} width={40} height={12} rx={6} />
    ) : (
      <path d="M220 291A43.5 43.5 0 0 0 280 291" {...cut} stroke-width={12} />
    );
  return (
    <>
      {eye(160)}
      {eye(340)}
      {mouth}
    </>
  );
}

/** Under 40px the mark is redrawn at 28x18 with stroked gills and larger features, so each expression stays distinct. */
function miniFeatures(eyes: Eyes): JSX.Element {
  const smile = <path d="M12.5 15a2.1 2.1 0 0 0 3 0" {...cut} stroke-width={1.2} />;
  if (eyes === 'closed') {
    return (
      <>
        <rect x={7.1} y={10.8} width={4.6} height={1.8} rx={0.9} />
        <rect x={16.3} y={10.8} width={4.6} height={1.8} rx={0.9} />
        <rect x={12.9} y={14.7} width={2.2} height={1.1} rx={0.55} />
      </>
    );
  }
  if (eyes === 'x') {
    return (
      <>
        <path d="M7.7 9.9l3.4 3.4m0-3.4l-3.4 3.4M16.9 9.9l3.4 3.4m0-3.4l-3.4 3.4" {...cut} stroke-width={1.7} />
        <path d="M12.6 15.9a2 2 0 0 1 2.8 0" {...cut} stroke-width={1.2} />
      </>
    );
  }
  if (eyes === 'happy') {
    return (
      <>
        <path d="M7.4 13.1l2-2.4 2 2.4M16.6 13.1l2-2.4 2 2.4" {...cut} stroke-width={1.7} />
        {smile}
      </>
    );
  }
  if (eyes === 'wide') {
    return (
      <>
        <rect x={7.2} y={9.4} width={4.4} height={4.4} rx={1.375} />
        <rect x={16.4} y={7.6} width={4.4} height={6.2} rx={1.375} />
        <rect x={12.4} y={15} width={3.2} height={1.1} rx={0.55} />
      </>
    );
  }
  return (
    <>
      <rect x={7.2} y={9.4} width={4.4} height={4.4} rx={1.375} />
      <rect x={16.4} y={9.4} width={4.4} height={4.4} rx={1.375} />
      {smile}
    </>
  );
}

/** The mascot, standing where a generic status icon would. A neutral state shows no face at all. */
export function Face({ eyes = 'open', size = 28, tone }: { eyes?: Eyes; size?: number; tone?: FaceTone }): JSX.Element {
  const mask = useId();
  const mini = size < 40;
  const width = mini ? Math.max(28, size) : size;
  const [box, ratio] = mini ? ['0 0 28 18', 18 / 28] : ['14 70 472 318', 318 / 472];
  return (
    <svg
      class="cp-face"
      data-tone={tone}
      width={width}
      height={Math.round(width * ratio)}
      viewBox={box}
      aria-hidden="true"
    >
      <mask id={mask}>
        <rect x={0} y={0} width={500} height={500} fill="#fff" />
        {mini ? miniFeatures(eyes) : features(eyes)}
      </mask>
      {mini ? (
        <>
          <path
            d="M9 8.2 6.7 1.9M6.9 10.3 1.9 7M6.5 13.1 1.3 13.2M19 8.2 21.3 1.9M21.1 10.3 26.1 7M21.5 13.1 26.7 13.2"
            fill="none"
            stroke="currentColor"
            stroke-width={2.2}
            stroke-linecap="round"
          />
          <path
            d="M14 5.83C20.29 5.83 23.25 7.69 23.25 11.64C23.25 16.42 21.03 17.93 14 17.93C6.97 17.93 4.75 16.42 4.75 11.64C4.75 7.69 7.71 5.83 14 5.83Z"
            fill="currentColor"
            mask={`url(#${mask})`}
          />
        </>
      ) : (
        <g fill="currentColor">
          <path d={GILLS} />
          <path d={HEAD} mask={`url(#${mask})`} />
        </g>
      )}
    </svg>
  );
}
