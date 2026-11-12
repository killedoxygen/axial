export interface DefaultSkin {
  id: string;
  name: string;
  variant: 'classic' | 'slim';
  src: string;
}

export const DEFAULT_SKINS: DefaultSkin[] = [
  {
    id: 'steve',
    name: 'Steve',
    variant: 'classic',
    src: 'skins/steve.png',
  },
  {
    id: 'alex',
    name: 'Alex',
    variant: 'slim',
    src: 'skins/alex.png',
  },
  {
    id: 'noor',
    name: 'Noor',
    variant: 'slim',
    src: 'skins/noor.png',
  },
  {
    id: 'sunny',
    name: 'Sunny',
    variant: 'classic',
    src: 'skins/sunny.png',
  },
  {
    id: 'ari',
    name: 'Ari',
    variant: 'slim',
    src: 'skins/ari.png',
  },
  {
    id: 'zuri',
    name: 'Zuri',
    variant: 'classic',
    src: 'skins/zuri.png',
  },
  {
    id: 'makena',
    name: 'Makena',
    variant: 'slim',
    src: 'skins/makena.png',
  },
  {
    id: 'kai',
    name: 'Kai',
    variant: 'classic',
    src: 'skins/kai.png',
  },
  {
    id: 'efe',
    name: 'Efe',
    variant: 'slim',
    src: 'skins/efe.png',
  },
];
