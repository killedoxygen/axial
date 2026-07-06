import type { JSX } from 'preact';

const LOGO_PATH =
  'M118 58h60a60 60 0 0160 60v60a84 84 0 0084 84h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a84 84 0 00-84-84h-60a60 60 0 01-60-60v-60a60 60 0 0160-60zm244 318v28a14 14 0 0014 14h28a14 14 0 0014-14v-28a14 14 0 00-14-14h-28a14 14 0 00-14 14zm-40-318h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a60 60 0 0160-60zm-204 204h60a60 60 0 0160 60v60a60 60 0 01-60 60h-60a60 60 0 01-60-60v-60a60 60 0 0160-60z';

export function Logo({
  className,
  size = 32,
  style,
}: {
  className?: string;
  size?: number;
  style?: JSX.CSSProperties;
}): JSX.Element {
  return (
    <svg
      class={className}
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
      <path fill="#D4FF26" fillRule="evenodd" d={LOGO_PATH} />
    </svg>
  );
}
