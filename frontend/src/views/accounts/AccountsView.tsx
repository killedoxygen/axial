import type { JSX } from 'preact';
import { useState } from 'preact/hooks';
import { Button, Card, Input, Pill, SectionHeading } from '../../ui/Atoms';
import { Icon } from '../../ui/Icons';
import { useTheme } from '../../hooks/use-theme';
import { config } from '../../store';
import { api } from '../../api';
import { toast } from '../../toast';
import { errMessage } from '../../utils';

// Accounts page for now is just player name plus placeholders
// Microsoft auth is not wired yet
export function AccountsView(): JSX.Element {
  const theme = useTheme();
  const cfg = config.value;
  const [username, setUsername] = useState(cfg?.username || 'Player');
  const [saving, setSaving] = useState(false);

  const save = async (): Promise<void> => {
    const u = username.trim();
    if (!u || u === cfg?.username) return;
    setSaving(true);
    try {
      const res: any = await api('PUT', '/config', { username: u });
      if (res.error) throw new Error(res.error);
      config.value = res;
      toast('Player name updated');
    } catch (err) {
      toast(`Failed: ${errMessage(err)}`);
    } finally {
      setSaving(false);
    }
  };

  const initial = username[0]?.toUpperCase() || 'P';

  return (
    <div class="cp-view-page" style={{ gap: 20 }}>
      <div class="cp-page-header">
        <div>
          <h1>Accounts & skins</h1>
          <div class="cp-page-sub">Player identity and account links.</div>
        </div>
      </div>

      <Card>
        <SectionHeading eyebrow="Player" title="Identity" />
        <div style={{ display: 'flex', alignItems: 'center', gap: 18, flexWrap: 'wrap' }}>
          <div style={{
            width: 96, height: 96, borderRadius: theme.r.lg,
            background: `linear-gradient(135deg, ${theme.accent.base}, ${theme.accent.strong})`,
            color: theme.accent.on,
            display: 'inline-flex', alignItems: 'center', justifyContent: 'center',
            fontSize: 40, fontWeight: 700, letterSpacing: -1,
            flexShrink: 0,
          }}>{initial}</div>
          <div style={{ flex: 1, minWidth: 240 }}>
            <div style={{
              fontSize: 11, fontWeight: 600, color: theme.n.textMute,
              textTransform: 'uppercase', letterSpacing: 0.8, marginBottom: 6,
            }}>Player name</div>
            <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <Input value={username} onChange={setUsername} placeholder="Player" style={{ maxWidth: 360 }} />
              <Button onClick={save} disabled={saving || !username.trim() || username === cfg?.username}>
                Save
              </Button>
            </div>
          </div>
        </div>
      </Card>

      <Card>
        <SectionHeading eyebrow="Account" title="Microsoft link" right={<Pill tone="warn">Not implemented</Pill>} />
        <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5 }}>
          Microsoft account sign-in for online play will arrive in a later pass. For now, launches use offline auth with the player name above.
        </div>
      </Card>

      <Card>
        <SectionHeading eyebrow="Skins" title="Skin library" right={<Pill tone="warn">Coming soon</Pill>} />
        <div style={{ fontSize: 13, color: theme.n.textDim, lineHeight: 1.5 }}>
          Skin management hasn't been built yet. Drop skins into an instance folder and Minecraft will pick them up directly.
        </div>
      </Card>
    </div>
  );
}
