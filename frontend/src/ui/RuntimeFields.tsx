import type { JSX } from 'preact';
import { useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { SelectField } from './Select';
import { Icon } from './Icons';
import { api } from '../api';

type JavaRuntime = { path: string; component: string; source: string };

const MANAGED_VALUE = '';
const CUSTOM_VALUE = '__custom__';

let runtimeCache: JavaRuntime[] | null = null;
let runtimeRequest: Promise<JavaRuntime[]> | null = null;

async function loadRuntimes(): Promise<JavaRuntime[]> {
  if (runtimeCache) return runtimeCache;
  runtimeRequest ??= (async () => {
    try {
      const res = await api('GET', '/java');
      const list = Array.isArray(res?.runtimes) ? (res.runtimes as JavaRuntime[]) : [];
      const valid = list.filter((runtime) => runtime?.path);
      if (valid.length > 0) runtimeCache = valid;
      return valid;
    } catch {
      return [];
    } finally {
      runtimeRequest = null;
    }
  })();
  return runtimeRequest;
}

function runtimeLabel(runtime: JavaRuntime): string {
  const major = /(\d+)/.exec(runtime.component)?.[1];
  const name = major ? `Java ${major}` : runtime.component || 'Java runtime';
  const tail = runtime.path.split(/[\\/]/).filter(Boolean).slice(-2).join('/');
  return tail ? `${name} · …/${tail}` : name;
}

export function JavaPathField({
  value,
  onChange,
  onCommit,
  disabled,
  className,
  label = 'Java runtime',
}: {
  value: string;
  onChange: (value: string) => void;
  onCommit?: (value: string) => void;
  disabled?: boolean;
  className?: string;
  label?: string;
}): JSX.Element {
  const [runtimes, setRuntimes] = useState<JavaRuntime[]>(runtimeCache ?? []);
  const trimmed = value.trim();
  const matchesDetected = runtimes.some((runtime) => runtime.path === trimmed);
  const isCustom = trimmed.length > 0 && !matchesDetected;
  const [customOpen, setCustomOpen] = useState(isCustom);

  useEffect(() => {
    if (runtimeCache) return;
    let alive = true;
    void loadRuntimes().then((list) => {
      if (alive && list.length > 0) setRuntimes(list);
    });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (isCustom) setCustomOpen(true);
  }, [isCustom]);

  const selectValue = customOpen || isCustom ? CUSTOM_VALUE : matchesDetected ? trimmed : MANAGED_VALUE;

  const options = useMemo(() => {
    const base = [{ value: MANAGED_VALUE, label: 'Managed (automatic)' }];
    const detected = runtimes.map((runtime) => ({ value: runtime.path, label: runtimeLabel(runtime) }));
    return [...base, ...detected, { value: CUSTOM_VALUE, label: 'Custom path…' }];
  }, [runtimes]);

  const handleSelect = (next: string): void => {
    if (next === CUSTOM_VALUE) {
      setCustomOpen(true);
      return;
    }
    setCustomOpen(false);
    onChange(next);
    onCommit?.(next);
  };

  return (
    <label class={`cp-ovr-field${className ? ` ${className}` : ''}`}>
      {label && <span>{label}</span>}
      <SelectField
        value={selectValue}
        disabled={disabled}
        ariaLabel="Java runtime"
        onChange={handleSelect}
        options={options}
      />
      {(customOpen || isCustom) && (
        <div class="cp-ovr-input">
          <Icon name="folder" size={14} color="var(--text-mute)" />
          <input
            type="text"
            value={value}
            placeholder="/path/to/java"
            autocomplete="off"
            spellcheck={false}
            disabled={disabled}
            aria-label="Custom Java path"
            onInput={(event) => onChange((event.currentTarget as HTMLInputElement).value)}
            onBlur={() => onCommit?.(value.trim())}
            onKeyDown={(event) => {
              if (event.key === 'Enter') (event.currentTarget as HTMLInputElement).blur();
            }}
          />
        </div>
      )}
    </label>
  );
}

type ArgKind = 'memory' | 'gc' | 'system' | 'module' | 'assert' | 'tuning' | 'unknown';

type ArgInfo = { kind: ArgKind; label: string };

function classifyArg(token: string): ArgInfo {
  if (/^-Xm[sx]\S*/i.test(token)) return { kind: 'memory', label: 'Heap size' };
  if (/^-X(ss|mn)\S*/i.test(token)) return { kind: 'memory', label: 'Memory tuning' };
  if (/^-XX:[+-]?Use\w*GC$/i.test(token)) return { kind: 'gc', label: 'Garbage collector' };
  if (/^-XX:/.test(token)) return { kind: 'tuning', label: 'JVM tuning flag' };
  if (/^-D\S+/.test(token)) return { kind: 'system', label: 'System property' };
  if (/^--(add|enable|illegal|patch)-\S+/.test(token)) return { kind: 'module', label: 'Module flag' };
  if (/^-(ea|da|enableassertions|disableassertions)\b/.test(token)) return { kind: 'assert', label: 'Assertions' };
  return { kind: 'unknown', label: 'Custom flag' };
}

type Suggestion = { insert: string; label: string; desc: string; template?: boolean };

const SUGGESTIONS: Suggestion[] = [
  { insert: '-Xmx', label: '-Xmx<size>', desc: 'Maximum heap, e.g. -Xmx4G', template: true },
  { insert: '-Xms', label: '-Xms<size>', desc: 'Initial heap, e.g. -Xms1G', template: true },
  { insert: '-XX:+UseG1GC', label: '-XX:+UseG1GC', desc: 'G1 garbage collector' },
  { insert: '-XX:+UseZGC', label: '-XX:+UseZGC', desc: 'Low-pause ZGC' },
  { insert: '-XX:+UseShenandoahGC', label: '-XX:+UseShenandoahGC', desc: 'Low-pause Shenandoah GC' },
  { insert: '-XX:MaxGCPauseMillis=', label: '-XX:MaxGCPauseMillis=<ms>', desc: 'Target GC pause', template: true },
  { insert: '-XX:+AlwaysPreTouch', label: '-XX:+AlwaysPreTouch', desc: 'Pre-touch heap pages at start' },
  { insert: '-Dfile.encoding=UTF-8', label: '-Dfile.encoding=UTF-8', desc: 'Force UTF-8 encoding' },
  { insert: '-Dsun.java2d.opengl=true', label: '-Dsun.java2d.opengl=true', desc: 'Enable OpenGL pipeline' },
  { insert: '-ea', label: '-ea', desc: 'Enable assertions' },
];

function tokenize(value: string): string[] {
  return value.trim().length ? value.trim().split(/\s+/) : [];
}

export function JvmArgsInput({
  value,
  onChange,
  disabled,
  label = 'Extra JVM arguments',
}: {
  value: string;
  onChange: (value: string) => void;
  disabled?: boolean;
  label?: string;
}): JSX.Element {
  const tokens = useMemo(() => tokenize(value), [value]);
  const [draft, setDraft] = useState('');
  const [focused, setFocused] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  const commit = (next: string[]): void => onChange(next.join(' '));

  const addToken = (token: string): void => {
    const clean = token.trim();
    if (!clean) return;
    commit([...tokens, clean]);
    setDraft('');
  };

  const removeAt = (index: number): void => {
    commit(tokens.filter((_, i) => i !== index));
  };

  const editAt = (index: number): void => {
    setDraft(tokens[index]);
    commit(tokens.filter((_, i) => i !== index));
    inputRef.current?.focus();
  };

  const suggestions = useMemo(() => {
    const q = draft.trim().toLowerCase();
    if (!focused) return [];
    const pool = SUGGESTIONS.filter((s) => !tokens.includes(s.insert));
    if (!q) return pool.slice(0, 6);
    return pool.filter((s) => s.insert.toLowerCase().includes(q) || s.label.toLowerCase().includes(q)).slice(0, 6);
  }, [draft, focused, tokens]);

  const applySuggestion = (suggestion: Suggestion): void => {
    if (suggestion.template) {
      setDraft(suggestion.insert);
      inputRef.current?.focus();
      return;
    }
    addToken(suggestion.insert);
    inputRef.current?.focus();
  };

  return (
    <div class="cp-ovr-field cp-ovr-args">
      {label && <span>{label}</span>}
      <div class={`cp-argbox${focused ? ' cp-argbox--focus' : ''}`} data-disabled={disabled ? 'true' : 'false'}>
        <div class="cp-argbox-tokens" onMouseDown={() => inputRef.current?.focus()}>
          {tokens.map((token, index) => {
            const info = classifyArg(token);
            return (
              <span key={`${token}-${index}`} class="cp-argchip" data-kind={info.kind} title={info.label}>
                <button
                  type="button"
                  class="cp-argchip-text"
                  disabled={disabled}
                  onClick={() => editAt(index)}
                  title={`${info.label}; click to edit`}
                >
                  {token}
                </button>
                <button
                  type="button"
                  class="cp-argchip-x"
                  aria-label={`Remove ${token}`}
                  disabled={disabled}
                  onClick={() => removeAt(index)}
                >
                  <Icon name="x" size={11} />
                </button>
              </span>
            );
          })}
          <input
            ref={inputRef}
            type="text"
            class="cp-argbox-input"
            value={draft}
            placeholder={tokens.length ? '' : '-Xmx4G -XX:+UseG1GC ...'}
            autocomplete="off"
            spellcheck={false}
            disabled={disabled}
            aria-label="Add JVM argument"
            onInput={(event) => setDraft((event.currentTarget as HTMLInputElement).value)}
            onFocus={() => setFocused(true)}
            onBlur={() => {
              window.setTimeout(() => setFocused(false), 120);
              addToken(draft);
            }}
            onKeyDown={(event) => {
              if (event.key === ' ' || event.key === 'Enter' || event.key === 'Tab') {
                if (draft.trim()) {
                  event.preventDefault();
                  addToken(draft);
                }
              } else if (event.key === 'Backspace' && !draft && tokens.length) {
                event.preventDefault();
                editAt(tokens.length - 1);
              }
            }}
          />
        </div>
        {suggestions.length > 0 && (
          <div class="cp-argbox-suggest" role="listbox">
            {suggestions.map((suggestion) => (
              <button
                key={suggestion.insert}
                type="button"
                class="cp-argbox-suggest-row"
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => applySuggestion(suggestion)}
              >
                <code>{suggestion.label}</code>
                <span>{suggestion.desc}</span>
              </button>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
