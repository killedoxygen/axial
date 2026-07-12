import type { JSX, ComponentChildren } from 'preact';
import { openExternalURL } from '../../native';

type Block =
  | { type: 'heading'; level: number; text: string }
  | { type: 'paragraph'; text: string }
  | { type: 'list'; ordered: boolean; items: string[] }
  | { type: 'quote'; text: string }
  | { type: 'code'; text: string }
  | { type: 'rule' };

/**
 * Project descriptions are author-written Markdown that may embed raw HTML, so
 * nothing here is ever handed to innerHTML: tags are stripped and the rest is
 * turned into elements we construct ourselves.
 */
export function ProjectBody({ body }: { body: string }): JSX.Element | null {
  const blocks = parse(body);
  if (blocks.length === 0) return null;
  return <div class="cp-content-body">{blocks.map(renderBlock)}</div>;
}

function parse(source: string): Block[] {
  const text = source
    .replace(/<br\s*\/?>/gi, '\n')
    .replace(/<\/(p|div|li|h[1-6]|tr)>/gi, '\n')
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/<[^>]*>/g, '')
    .replace(/\r\n/g, '\n');

  const lines = text.split('\n');
  const blocks: Block[] = [];
  let paragraph: string[] = [];
  let list: { ordered: boolean; items: string[] } | null = null;
  let code: string[] | null = null;

  const flushParagraph = (): void => {
    const joined = paragraph.join(' ').trim();
    if (joined) blocks.push({ type: 'paragraph', text: joined });
    paragraph = [];
  };
  const flushList = (): void => {
    if (list && list.items.length > 0) blocks.push({ type: 'list', ...list });
    list = null;
  };
  const flush = (): void => {
    flushParagraph();
    flushList();
  };

  for (const raw of lines) {
    const line = raw.trimEnd();

    if (line.trimStart().startsWith('```')) {
      if (code) {
        blocks.push({ type: 'code', text: code.join('\n') });
        code = null;
      } else {
        flush();
        code = [];
      }
      continue;
    }
    if (code) {
      code.push(raw);
      continue;
    }

    if (!line.trim()) {
      flush();
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(line.trim());
    if (heading) {
      flush();
      blocks.push({ type: 'heading', level: heading[1].length, text: heading[2].trim() });
      continue;
    }

    if (/^(\*{3,}|-{3,}|_{3,})$/.test(line.trim())) {
      flush();
      blocks.push({ type: 'rule' });
      continue;
    }

    const quote = /^>\s?(.*)$/.exec(line.trim());
    if (quote) {
      flush();
      blocks.push({ type: 'quote', text: quote[1].trim() });
      continue;
    }

    const bullet = /^[-*+]\s+(.*)$/.exec(line.trim());
    const ordered = /^\d+[.)]\s+(.*)$/.exec(line.trim());
    if (bullet || ordered) {
      flushParagraph();
      const isOrdered = !bullet;
      if (!list || list.ordered !== isOrdered) {
        flushList();
        list = { ordered: isOrdered, items: [] };
      }
      list.items.push(((bullet ?? ordered) as RegExpExecArray)[1].trim());
      continue;
    }

    flushList();
    paragraph.push(line.trim());
  }

  if (code) blocks.push({ type: 'code', text: code.join('\n') });
  flush();
  return blocks;
}

function renderBlock(block: Block, index: number): JSX.Element {
  switch (block.type) {
    case 'heading':
      return block.level <= 2 ? <h3 key={index}>{inline(block.text)}</h3> : <h4 key={index}>{inline(block.text)}</h4>;
    case 'list':
      return block.ordered ? (
        <ol key={index}>
          {block.items.map((item, i) => (
            <li key={i}>{inline(item)}</li>
          ))}
        </ol>
      ) : (
        <ul key={index}>
          {block.items.map((item, i) => (
            <li key={i}>{inline(item)}</li>
          ))}
        </ul>
      );
    case 'quote':
      return <blockquote key={index}>{inline(block.text)}</blockquote>;
    case 'code':
      return (
        <pre key={index}>
          <code>{block.text}</code>
        </pre>
      );
    case 'rule':
      return <hr key={index} />;
    default:
      return <p key={index}>{inline(block.text)}</p>;
  }
}

const INLINE = /(\[[^\]]+\]\([^)\s]+\))|(`[^`]+`)|(\*\*[^*]+\*\*)|(__[^_]+__)|(\*[^*\n]+\*)|(_[^_\n]+_)/g;

function inline(text: string): ComponentChildren {
  const nodes: ComponentChildren[] = [];
  let last = 0;
  let match: RegExpExecArray | null;
  INLINE.lastIndex = 0;

  while ((match = INLINE.exec(text)) !== null) {
    if (match.index > last) nodes.push(text.slice(last, match.index));
    const token = match[0];
    const key = nodes.length;

    if (token.startsWith('[')) {
      const link = /^\[([^\]]+)\]\(([^)\s]+)\)$/.exec(token);
      if (link && /^https?:\/\//i.test(link[2])) nodes.push(<ExternalLink key={key} href={link[2]} label={link[1]} />);
      else if (link) nodes.push(link[1]);
    } else if (token.startsWith('`')) {
      nodes.push(<code key={key}>{token.slice(1, -1)}</code>);
    } else if (token.startsWith('**') || token.startsWith('__')) {
      nodes.push(<strong key={key}>{token.slice(2, -2)}</strong>);
    } else {
      nodes.push(<em key={key}>{token.slice(1, -1)}</em>);
    }
    last = match.index + token.length;
  }

  if (last < text.length) nodes.push(text.slice(last));
  return nodes;
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
