import type { JSX } from 'preact';

export interface MicrosoftMarkProps {
  size?: number;
  class?: string;
}

export function MicrosoftMark({ size = 16, class: className }: MicrosoftMarkProps): JSX.Element {
  return (
    <svg
      class={className}
      width={size}
      height={size}
      viewBox="0 0 16 16"
      aria-hidden="true"
      focusable="false"
      style={{ display: 'block', flexShrink: 0, borderRadius: 2 }}
    >
      <rect x="0" y="0" width="7" height="7" fill="#f25022" />
      <rect x="9" y="0" width="7" height="7" fill="#7fba00" />
      <rect x="0" y="9" width="7" height="7" fill="#00a4ef" />
      <rect x="9" y="9" width="7" height="7" fill="#ffb900" />
    </svg>
  );
}
