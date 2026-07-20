import type { JSX } from 'preact';
import {
  Activity,
  Archive,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Box,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  CircleAlert,
  CircleCheck,
  CircleDashed,
  Clock,
  Compass,
  Copy,
  Download,
  Ellipsis,
  Folder,
  Globe,
  Headphones,
  House,
  Image,
  Info,
  Keyboard,
  Layers,
  Maximize2,
  Minus,
  Music,
  Palette,
  Pencil,
  Play,
  Plus,
  Puzzle,
  RefreshCw,
  Search,
  Settings,
  ShieldCheck,
  ShieldUser,
  SkipForward,
  SlidersHorizontal,
  Square,
  SquareStop,
  Tag,
  Terminal,
  Trash2,
  User,
  Volume2,
  VolumeOff,
  VolumeX,
  X,
  type LucideIcon,
} from 'lucide-preact';

const REGISTRY = {
  activity: Activity,
  archive: Archive,
  'arrow-left': ArrowLeft,
  'arrow-right': ArrowRight,
  'arrow-up': ArrowUp,
  check: Check,
  'check-circle': CircleCheck,
  'chevron-left': ChevronLeft,
  'chevron-right': ChevronRight,
  'chevron-down': ChevronDown,
  'chevron-up': ChevronUp,
  'circle-dashed': CircleDashed,
  clock: Clock,
  compass: Compass,
  copy: Copy,
  cube: Box,
  dots: Ellipsis,
  download: Download,
  edit: Pencil,
  expand: Maximize2,
  folder: Folder,
  globe: Globe,
  headphones: Headphones,
  home: House,
  image: Image,
  info: Info,
  alert: CircleAlert,
  keyboard: Keyboard,
  minus: Minus,
  music: Music,
  'music-off': VolumeOff,
  palette: Palette,
  puzzle: Puzzle,
  play: Play,
  'player-skip': SkipForward,
  plus: Plus,
  rectangle: Square,
  refresh: RefreshCw,
  search: Search,
  settings: Settings,
  sliders: SlidersHorizontal,
  'shield-check': ShieldCheck,
  'shield-person': ShieldUser,
  stack: Layers,
  stop: SquareStop,
  tag: Tag,
  terminal: Terminal,
  trash: Trash2,
  user: User,
  volume: Volume2,
  'volume-off': VolumeX,
  x: X,
} as const satisfies Record<string, LucideIcon>;

export type IconName = keyof typeof REGISTRY;

export const ICON_NAMES: readonly IconName[] = Object.freeze(Object.keys(REGISTRY) as IconName[]);

export function isIconName(value: string): value is IconName {
  return Object.prototype.hasOwnProperty.call(REGISTRY, value);
}

export interface IconProps {
  name: IconName;
  size?: number;
  stroke?: number;
  color?: string;
  style?: JSX.CSSProperties;
}

export function Icon({ name, size = 18, stroke = 2, color = 'currentColor', style }: IconProps): JSX.Element {
  const Component = REGISTRY[name];
  return (
    <Component
      size={size}
      strokeWidth={stroke}
      color={color}
      aria-hidden={true}
      focusable="false"
      style={{ display: 'block', flexShrink: 0, ...style }}
    />
  );
}
