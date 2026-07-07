import type { JSX } from 'preact';

export type LogoMotion = 'none' | 'loose' | 'assembly';

const LOGO_RIBBON_PATH =
  'M118 58h60a60 60 0 0160 60v60a84 84 0 0084 84h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a84 84 0 00-84-84h-60a60 60 0 01-60-60v-60a60 60 0 0160-60zM362 376v28a14 14 0 0014 14h28a14 14 0 0014-14v-28a14 14 0 00-14-14h-28a14 14 0 00-14 14z';
const LOGO_TOP_RIGHT_PATH = 'M322 58h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a60 60 0 0160-60z';
const LOGO_BOTTOM_LEFT_PATH =
  'M118 262h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a60 60 0 0160-60z';

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
      viewBox="0 0 500 500"
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
        <path fill="#D4FF26" fillRule="evenodd" d={LOGO_RIBBON_PATH} />
      </g>
      <g class="cp-mark-tr">
        <path fill="#D4FF26" d={LOGO_TOP_RIGHT_PATH} />
      </g>
      <g class="cp-mark-bl">
        <path fill="#D4FF26" d={LOGO_BOTTOM_LEFT_PATH} />
      </g>
    </svg>
  );
}
