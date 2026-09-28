import type { JSX } from 'preact';

import brandMark from '../../../assets/brand-mark.json';

export type LogoMotion = 'none' | 'loose' | 'assembly';

const viewBox = brandMark.view_box.join(' ');

export function Logo({
  className,
  motion = 'none',
  size = 32,
  style,
}: {
  className?: string;
  motion?: LogoMotion;
  size?: number;
  style?: JSX.CSSProperties;
}): JSX.Element {
  const classes = ['cp-mark', motion !== 'none' && `cp-mark--${motion}`, className].filter(Boolean).join(' ');

  return (
    <svg
      class={classes}
      width={size}
      height={size}
      viewBox={viewBox}
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
      focusable="false"
      style={{
        width: size,
        height: size,
        filter: 'var(--logo-filter, none)',
        ...style,
      }}
    >
      <g class="cp-mark-ribbon">
        <path fill={brandMark.colors.interface} fillRule="evenodd" d={brandMark.paths.ribbon} />
      </g>
      <g class="cp-mark-tr">
        <path fill={brandMark.colors.interface} d={brandMark.paths.top_right} />
      </g>
      <g class="cp-mark-bl">
        <path fill={brandMark.colors.interface} d={brandMark.paths.bottom_left} />
      </g>
    </svg>
  );
}
