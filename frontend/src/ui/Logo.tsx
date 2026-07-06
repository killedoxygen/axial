import type { JSX } from 'preact';

const LOGO_PATH =
  'M9 2h2v1H9z M13 2h1v1H13z M3 3h5v1H3z M10 3h2v1H10z M3 4h7v1H3z M11 4h2v1H11z M3 5h8v1H3z M2 6h4v1H2z M10 6h3v1H10z M2 7h4v1H2z M10 7h3v1H10z M2 8h4v1H2z M10 8h4v1H10z M3 9h3v1H3z M10 9h4v1H10z M3 10h3v1H3z M10 10h4v1H10z M3 11h11v1H3z M2 12h11v1H2z M2 13h3v1H2z M7 13h6v1H7z';

export function Logo({
  className,
  size = 26,
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
      viewBox="0 0 16 16"
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
      <path shapeRendering="crispEdges" fill="#b3e029" fillRule="evenodd" d={LOGO_PATH} />
    </svg>
  );
}
