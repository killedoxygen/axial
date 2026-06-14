import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { classifyLogText, logLineMatchesFilter } from '../logs';
import type { ClassifiedLogLine, LogFilter } from '../logs';

export function LogLine({ line, compact = false }: { line: ClassifiedLogLine; compact?: boolean }): JSX.Element {
  return (
    <div class={`cp-log-line${compact ? ' cp-log-line--compact' : ''}`} data-kind={line.kind}>
      <span class="cp-log-line-label" aria-label={`${line.kind} log line`}>
        {line.label}
      </span>
      <span class="cp-log-line-text">{line.text || ' '}</span>
    </div>
  );
}

export function LogLines({ text, filter }: { text: string; filter: LogFilter }): JSX.Element {
  const lines = useMemo(() => classifyLogText(text), [text]);
  const filteredLines = useMemo(() => lines.filter((line) => logLineMatchesFilter(line, filter)), [filter, lines]);

  if (lines.length === 0) {
    return <div class="cp-log-empty">Log file is empty.</div>;
  }
  if (filteredLines.length === 0) {
    return <div class="cp-log-empty">No lines match this filter.</div>;
  }
  return (
    <div class="cp-log-lines" role="log" aria-label="Log preview">
      {filteredLines.map((line) => (
        <LogLine line={line} key={line.index} />
      ))}
    </div>
  );
}
