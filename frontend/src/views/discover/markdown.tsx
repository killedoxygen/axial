import type { ComponentChildren, JSX } from 'preact';
import type { ReactElement, ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { openExternalURL } from '../../native';

/**
 * Project descriptions are untrusted, author-written Markdown. ReactMarkdown
 * builds elements instead of using innerHTML; raw HTML and remote images stay
 * disabled, while GFM covers the syntax commonly used by content providers.
 */
export function ProjectBody({ body }: { body: string }): JSX.Element | null {
  if (!body.trim()) return null;

  return (
    <div class="cp-content-body">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        components={{
          a: MarkdownLink,
          h1: MajorHeading,
          h2: MajorHeading,
          h3: MinorHeading,
          h4: MinorHeading,
          h5: MinorHeading,
          h6: MinorHeading,
          img: () => null,
        }}
      >
        {body}
      </ReactMarkdown>
    </div>
  );
}

function MajorHeading({ children }: { children?: ReactNode }): ReactElement {
  return (<h3>{children as ComponentChildren}</h3>) as unknown as ReactElement;
}

function MinorHeading({ children }: { children?: ReactNode }): ReactElement {
  return (<h4>{children as ComponentChildren}</h4>) as unknown as ReactElement;
}

function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }): ReactElement {
  const element =
    href && isExternalURL(href) ? (
      <ExternalLink href={href}>{children as ComponentChildren}</ExternalLink>
    ) : (
      <>{children as ComponentChildren}</>
    );
  return element as unknown as ReactElement;
}

function isExternalURL(value: string): boolean {
  return /^https?:\/\//i.test(value);
}

export function ExternalLink({
  href,
  label,
  children,
  class: className,
}: {
  href: string;
  label?: string;
  children?: ComponentChildren;
  class?: string;
}): JSX.Element {
  return (
    <a
      href={href}
      class={className}
      onClick={(event: MouseEvent) => {
        event.preventDefault();
        void openExternalURL(href);
      }}
    >
      {children ?? label ?? href}
    </a>
  );
}
