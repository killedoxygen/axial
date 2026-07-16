import type { JSX, FunctionComponent } from 'preact';
import {
  Archive,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Check,
  CheckCircle,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  ChevronUp,
  CircleDashed,
  Clock,
  ColorTheme,
  Compass,
  Copy,
  Cube,
  DotsHorizontal,
  Download,
  Edit,
  ExclamationMarkCircle,
  Expand,
  Folder,
  Globe,
  Headphones,
  Home,
  ImageSquare,
  InfoCircle,
  Keyboard,
  Minus,
  Music,
  PlaySm as Play,
  PluginPuzzle,
  Plus,
  Pulse,
  Reload,
  Search,
  Settings,
  SettingsSlider,
  ShieldCheck,
  ShieldPerson,
  Skip,
  SoundOffSimpleMute,
  SoundOffSpeaker,
  SoundOnReadOutLoudSpeaker,
  Stack,
  SquareCheckboxUnchecked,
  Stop,
  Tag,
  Terminal,
  Trash,
  User,
  X,
} from '@openai/apps-sdk-ui/components/Icon';

type IconComponent = FunctionComponent<{
  width?: number | string;
  height?: number | string;
  color?: string;
  class?: string;
  'aria-hidden'?: boolean | 'true' | 'false';
  focusable?: boolean | 'true' | 'false';
  style?: JSX.CSSProperties;
}>;

const REGISTRY: Record<string, IconComponent> = {
  activity: Pulse as IconComponent,
  archive: Archive as IconComponent,
  'arrow-left': ArrowLeft as IconComponent,
  'arrow-right': ArrowRight as IconComponent,
  'arrow-up': ArrowUp as IconComponent,
  check: Check as IconComponent,
  'check-circle': CheckCircle as IconComponent,
  'chevron-left': ChevronLeft as IconComponent,
  'chevron-right': ChevronRight as IconComponent,
  'chevron-down': ChevronDown as IconComponent,
  'chevron-up': ChevronUp as IconComponent,
  'circle-dashed': CircleDashed as IconComponent,
  clock: Clock as IconComponent,
  compass: Compass as IconComponent,
  copy: Copy as IconComponent,
  cube: Cube as IconComponent,
  dots: DotsHorizontal as IconComponent,
  download: Download as IconComponent,
  edit: Edit as IconComponent,
  expand: Expand as IconComponent,
  folder: Folder as IconComponent,
  globe: Globe as IconComponent,
  headphones: Headphones as IconComponent,
  home: Home as IconComponent,
  image: ImageSquare as IconComponent,
  info: InfoCircle as IconComponent,
  alert: ExclamationMarkCircle as IconComponent,
  keyboard: Keyboard as IconComponent,
  minus: Minus as IconComponent,
  music: Music as IconComponent,
  'music-off': SoundOffSimpleMute as IconComponent,
  palette: ColorTheme as IconComponent,
  puzzle: PluginPuzzle as IconComponent,
  play: Play as IconComponent,
  'player-skip': Skip as IconComponent,
  plus: Plus as IconComponent,
  rectangle: SquareCheckboxUnchecked as IconComponent,
  refresh: Reload as IconComponent,
  search: Search as IconComponent,
  settings: Settings as IconComponent,
  sliders: SettingsSlider as IconComponent,
  'shield-check': ShieldCheck as IconComponent,
  'shield-person': ShieldPerson as IconComponent,
  stack: Stack as IconComponent,
  stop: Stop as IconComponent,
  tag: Tag as IconComponent,
  terminal: Terminal as IconComponent,
  trash: Trash as IconComponent,
  user: User as IconComponent,
  volume: SoundOnReadOutLoudSpeaker as IconComponent,
  'volume-off': SoundOffSpeaker as IconComponent,
  x: X as IconComponent,
};

export interface IconProps {
  name: string;
  size?: number;
  stroke?: number;
  color?: string;
  style?: JSX.CSSProperties;
}

export function Icon({
  name,
  size = 18,
  stroke: _stroke = 2,
  color = 'currentColor',
  style,
}: IconProps): JSX.Element | null {
  const Cmp = REGISTRY[name];
  if (!Cmp) return null;
  return (
    <Cmp
      width={size}
      height={size}
      color={color}
      aria-hidden={true}
      focusable={false}
      style={{ display: 'block', flexShrink: 0, ...style }}
    />
  );
}
