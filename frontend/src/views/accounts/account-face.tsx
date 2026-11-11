import type { JSX } from 'preact';
import { accountSkinSrc, accountSkinTextureSrc } from '../../player-skin';
import { Chip } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { MicrosoftMark } from '../../ui/MicrosoftMark';
import { PlayerHeadPreview } from '../../ui/PlayerHeadPreview';
import type { LauncherAccount } from './types';

// How an account is shown, apart from anything that can be done to it: the shell uses these without loading
// the account commands.

export function accountName(account: LauncherAccount): string {
  if (account.kind === 'offline') return account.display_name;
  return (account.minecraft_profile?.name ?? account.display_name) || 'Microsoft account';
}

/** The account's face. The one being played as follows the skin in hand. */
export function AccountHead({ account, size = 24 }: { account: LauncherAccount; size?: number }): JSX.Element {
  const texture = (account.active && accountSkinSrc.value) || accountSkinTextureSrc(account) || undefined;
  return <PlayerHeadPreview textureSrc={texture} size={size} radius={Math.round(size * 0.29)} />;
}

/** What kind of account it is, as its mark alone. */
export function AccountKindMark({ account }: { account: LauncherAccount }): JSX.Element {
  return account.kind === 'microsoft' ? <MicrosoftMark size={10} /> : <Icon name="user" size={11} />;
}

export function AccountKindChip({ account }: { account: LauncherAccount }): JSX.Element {
  return (
    <Chip>
      <AccountKindMark account={account} />
      {account.kind === 'microsoft' ? 'Microsoft' : 'Offline'}
    </Chip>
  );
}
