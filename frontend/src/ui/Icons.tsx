import type { JSX, FunctionComponent } from 'preact';
import {
  IconArrowLeft,
  IconArrowRight,
  IconCheck,
  IconBorderCornerRounded,
  IconChevronLeft,
  IconChevronRight,
  IconChevronUp,
  IconClock,
  IconCompass,
  IconCopy,
  IconCube,
  IconDots,
  IconDownload,
  IconEdit,
  IconFolder,
  IconGlobe,
  IconHeadphones,
  IconHome,
  IconInfoCircle,
  IconAlertCircle,
  IconKeyboard,
  IconMinus,
  IconMusic,
  IconMusicOff,
  IconPalette,
  IconPlayerPlay,
  IconPlayerSkipForward,
  IconPlayerStop,
  IconPlus,
  IconRectangle,
  IconRefresh,
  IconSearch,
  IconSettings,
  IconTag,
  IconTerminal2,
  IconTrash,
  IconUser,
  IconX,
} from '@tabler/icons-preact';

type IconComponent = FunctionComponent<{
  size?: number | string;
  stroke?: number | string;
  color?: string;
  class?: string;
  style?: JSX.CSSProperties;
}>;

const REGISTRY: Record<string, IconComponent> = {
  'border-corner-rounded': IconBorderCornerRounded as IconComponent,
  'arrow-left': IconArrowLeft as IconComponent,
  'arrow-right': IconArrowRight as IconComponent,
  'check': IconCheck as IconComponent,
  'chevron-left': IconChevronLeft as IconComponent,
  'chevron-right': IconChevronRight as IconComponent,
  'chevron-up': IconChevronUp as IconComponent,
  'clock': IconClock as IconComponent,
  'compass': IconCompass as IconComponent,
  'copy': IconCopy as IconComponent,
  'cube': IconCube as IconComponent,
  'dots': IconDots as IconComponent,
  'download': IconDownload as IconComponent,
  'edit': IconEdit as IconComponent,
  'folder': IconFolder as IconComponent,
  'globe': IconGlobe as IconComponent,
  'headphones': IconHeadphones as IconComponent,
  'home': IconHome as IconComponent,
  'info': IconInfoCircle as IconComponent,
  'alert': IconAlertCircle as IconComponent,
  'keyboard': IconKeyboard as IconComponent,
  'minus': IconMinus as IconComponent,
  'music': IconMusic as IconComponent,
  'music-off': IconMusicOff as IconComponent,
  'palette': IconPalette as IconComponent,
  'play': IconPlayerPlay as IconComponent,
  'player-skip': IconPlayerSkipForward as IconComponent,
  'plus': IconPlus as IconComponent,
  'rectangle': IconRectangle as IconComponent,
  'refresh': IconRefresh as IconComponent,
  'search': IconSearch as IconComponent,
  'settings': IconSettings as IconComponent,
  'stop': IconPlayerStop as IconComponent,
  'tag': IconTag as IconComponent,
  'terminal': IconTerminal2 as IconComponent,
  'trash': IconTrash as IconComponent,
  'user': IconUser as IconComponent,
  'x': IconX as IconComponent,
};

export interface IconProps {
  name: string;
  size?: number;
  stroke?: number;
  color?: string;
  style?: JSX.CSSProperties;
}

export function Icon({ name, size = 18, stroke = 2, color = 'currentColor', style }: IconProps): JSX.Element | null {
  const Cmp = REGISTRY[name];
  if (!Cmp) return null;
  return (
    <Cmp
      size={size}
      stroke={stroke}
      color={color}
      style={{ display: 'block', flexShrink: 0, ...style }}
    />
  );
}
