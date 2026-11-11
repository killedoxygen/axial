import type { JSX } from 'preact';
import { useMemo } from 'preact/hooks';
import { classifyLogText, logLineMatchesFilter } from '../logs';
import type { ClassifiedLogLine, LogFilter } from '../logs';

/** Minecraft's `[time] [thread/LEVEL]: message`; any other line is all message. */
const LINE = /^\[(\d{1,2}:\d{2}:\d{2}(?:\.\d+)?)\] \[([^\]]+)\]:? ?(.*)$/;

export function LogLine({ line }: { line: ClassifiedLogLine }): JSX.Element {
  const [, time = '', source = '', message = line.text] = LINE.exec(line.text) ?? [];
  return (
    <div class="cp-tr" data-kind={line.kind}>
      <span class="cp-log-time">{time}</span>
      <span class="cp-log-source" aria-label={`${line.kind} log line`}>
        {source}
      </span>
      <span class="cp-log-text">{message || ' '}</span>
    </div>
  );
}

export function LogLines({ text, filter }: { text: string; filter: LogFilter }): JSX.Element {
  const lines = useMemo(() => classifyLogText(text), [text]);
  const filteredLines = useMemo(() => lines.filter((line) => logLineMatchesFilter(line, filter)), [filter, lines]);

  if (lines.length === 0) {
    return <div class="cp-resource-note">Log file is empty.</div>;
  }
  if (filteredLines.length === 0) {
    return <div class="cp-resource-note">No lines match this filter.</div>;
  }
  return (
    <div class="cp-table cp-log" role="log" aria-label="Log preview">
      {filteredLines.map((line) => (
        <LogLine line={line} key={line.index} />
      ))}
    </div>
  );
}
