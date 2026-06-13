import type { JSX } from 'preact';
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { capeFileUrl } from './api';
import { NO_CAPE_VALUE, type MinecraftCape } from './types';

function capeLabel(cape: MinecraftCape, index: number): string {
  return cape.state.toLowerCase() === 'active' ? 'Active cape' : `Cape ${index + 1}`;
}

function capeShortId(capeId: string): string {
  return capeId.length > 16 ? `${capeId.slice(0, 8)}...${capeId.slice(-6)}` : capeId;
}

export function CapePicker({
  capes,
  value,
  onChange,
}: {
  capes: MinecraftCape[];
  value: string;
  onChange: (value: string) => void;
}): JSX.Element {
  const theme = useTheme();
  const pickerRef = useRef<HTMLDivElement>(null);
  const [scrollCue, setScrollCue] = useState({ top: false, bottom: false });
  const sortedCapes = useMemo(
    () => [...capes].sort((left, right) => {
      const leftActive = left.state.toLowerCase() === 'active';
      const rightActive = right.state.toLowerCase() === 'active';
      if (leftActive !== rightActive) return leftActive ? -1 : 1;
      return left.id.localeCompare(right.id);
    }),
    [capes],
  );
  const updateScrollCue = useCallback(() => {
    const node = pickerRef.current;
    if (!node) {
      setScrollCue({ top: false, bottom: false });
      return;
    }

    const scrollable = node.scrollHeight > node.clientHeight + 1;
    const next = {
      top: scrollable && node.scrollTop > 2,
      bottom: scrollable && node.scrollTop < node.scrollHeight - node.clientHeight - 2,
    };
    setScrollCue((current) => (
      current.top === next.top && current.bottom === next.bottom ? current : next
    ));
  }, []);

  useEffect(() => {
    updateScrollCue();
    const node = pickerRef.current;
    if (!node) return undefined;

    let resizeObserver: ResizeObserver | null = null;
    if (typeof ResizeObserver !== 'undefined') {
      resizeObserver = new ResizeObserver(updateScrollCue);
      resizeObserver.observe(node);
    }

    window.addEventListener('resize', updateScrollCue);
    return () => {
      resizeObserver?.disconnect();
      window.removeEventListener('resize', updateScrollCue);
    };
  }, [sortedCapes.length, updateScrollCue, value]);

  return (
    <div
      class="cp-cape-picker-frame"
      data-cape-picker-frame
      data-cape-picker-fade-top={scrollCue.top ? 'visible' : 'hidden'}
      data-cape-picker-fade-bottom={scrollCue.bottom ? 'visible' : 'hidden'}
    >
      <div
        ref={pickerRef}
        role="radiogroup"
        aria-label="Saved skin cape"
        class="cp-cape-picker"
        data-cape-picker
        data-cape-picker-overflow={sortedCapes.length > 5 ? 'bounded' : 'none'}
        onScroll={updateScrollCue}
      >
        <CapeChoiceButton
          label="No cape"
          caption="None"
          selected={value === NO_CAPE_VALUE}
          onClick={() => onChange(NO_CAPE_VALUE)}
        />
        {sortedCapes.map((cape, index) => {
          const active = cape.state.toLowerCase() === 'active';
          return (
            <CapeChoiceButton
              key={cape.id}
              label={capeLabel(cape, index)}
              caption={active ? 'Active' : capeShortId(cape.id)}
              selected={value === cape.id}
              imageSrc={capeFileUrl(cape)}
              onClick={() => onChange(cape.id)}
            />
          );
        })}
        {sortedCapes.length === 0 && (
          <div style={{
            alignSelf: 'center',
            color: theme.n.textMute,
            fontSize: 12,
            fontWeight: 500,
            lineHeight: 1.35,
          }}>
            No Minecraft capes on this profile.
          </div>
        )}
        {value !== NO_CAPE_VALUE && !sortedCapes.some((cape) => cape.id === value) && (
          <div style={{
            alignSelf: 'center',
            color: theme.n.textMute,
            fontSize: 12,
            fontWeight: 500,
            lineHeight: 1.35,
          }}>
            Saved cape is unavailable on this profile.
          </div>
        )}
      </div>
      <span class="cp-cape-picker__fade cp-cape-picker__fade--top" data-cape-picker-fade="top" aria-hidden="true" />
      <span class="cp-cape-picker__fade cp-cape-picker__fade--bottom" data-cape-picker-fade="bottom" aria-hidden="true" />
    </div>
  );
}

function CapeChoiceButton({
  label,
  caption,
  selected,
  imageSrc,
  onClick,
}: {
  label: string;
  caption: string;
  selected: boolean;
  imageSrc?: string;
  onClick: () => void;
}): JSX.Element {
  const theme = useTheme();
  return (
    <button
      type="button"
      role="radio"
      class="cp-cape-choice"
      aria-checked={selected}
      title={label}
      onClick={onClick}
      style={{
        width: 66,
        minHeight: 96,
        display: 'grid',
        justifyItems: 'center',
        gap: 5,
        margin: 0,
        padding: 5,
        border: selected ? '1px solid transparent' : '1px solid var(--line)',
        borderRadius: theme.r.sm,
        background: selected ? 'var(--accent-fill)' : theme.n.surface2,
        color: selected ? 'var(--accent-on)' : theme.n.text,
        boxShadow: selected ? 'var(--shadow-raised)' : undefined,
        cursor: 'pointer',
        font: 'inherit',
      }}
    >
      <span style={{
        position: 'relative',
        display: 'block',
        width: 44,
        aspectRatio: '10 / 16',
        overflow: 'hidden',
        borderRadius: theme.r.xs,
        background: selected
          ? 'color-mix(in oklab, var(--accent-on) 12%, transparent)'
          : 'color-mix(in oklab, var(--bg) 55%, var(--surface))',
      }}>
        {imageSrc ? (
          <img
            src={imageSrc}
            alt=""
            draggable={false}
            style={{
              position: 'absolute',
              width: '640%',
              height: '200%',
              left: '-10%',
              top: '-6.25%',
              objectFit: 'cover',
              imageRendering: 'pixelated',
              transform: 'scale(1.01)',
              transformOrigin: '7.8125% 25%',
            }}
          />
        ) : (
          <span style={{
            position: 'absolute',
            inset: 0,
            display: 'grid',
            placeItems: 'center',
          }}>
            <Icon name="x" size={18} />
          </span>
        )}
      </span>
      <span style={{
        maxWidth: '100%',
        overflow: 'hidden',
        textOverflow: 'ellipsis',
        whiteSpace: 'nowrap',
        fontSize: 11,
        fontWeight: 700,
        lineHeight: 1.1,
      }}>
        {caption}
      </span>
    </button>
  );
}
