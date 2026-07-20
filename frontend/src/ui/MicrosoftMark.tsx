import type { JSX } from 'preact';

export interface MicrosoftMarkProps {
  size?: number;
  class?: string;
}

export function MicrosoftMark({ size = 16, class: className }: MicrosoftMarkProps): JSX.Element {
  return (
    <img
      class={className}
      width={size}
      height={size}
      src="microsoft-auth-symbol.svg"
      alt=""
      aria-hidden="true"
      style={{ display: 'block', flexShrink: 0 }}
    />
  );
}
