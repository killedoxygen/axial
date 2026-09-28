import type { JSX } from 'preact';
import { useEffect, useState } from 'preact/hooks';
import { Toggle } from '../../ui/Atoms';
import { Slider } from '../../ui/Slider';
import { SettingRow, SettingsSection } from '../../ui/SettingsSheet';
import { local, saveLocalState } from '../../state';
import { Sound } from '../../sound';
import { Music, musicStateVersion } from '../../music';

export function AudioSection(): JSX.Element {
  musicStateVersion.value;
  const [soundsOn, setSoundsOn] = useState<boolean>(local.sounds);
  const [musicOn, setMusicOn] = useState<boolean>(Music.enabled);
  const [volume, setVolume] = useState<number>(Music.volume);

  useEffect(() => {
    setMusicOn(Music.enabled);
    setVolume(Music.volume);
  }, [musicStateVersion.value]);

  const toggleSounds = (): void => {
    const next = !soundsOn;
    setSoundsOn(next);
    local.sounds = next;
    Sound.enabled = next;
    saveLocalState();
    if (next) Sound.ui('affirm');
  };

  const toggleMusic = (): void => {
    Music.toggle();
    setMusicOn(Music.enabled);
  };

  return (
    <SettingsSection>
      <SettingRow
        title="UI sounds"
        description="Soft audio feedback for buttons, sliders, and theme changes."
        control={<Toggle on={soundsOn} onChange={toggleSounds} />}
      />
      <SettingRow
        title="Background music"
        description="Ambient track while you're in the launcher. Pauses automatically during gameplay."
        control={<Toggle on={musicOn} onChange={toggleMusic} />}
      />
      {musicOn && (
        <SettingRow title="Music volume" description={`Ambient level: ${volume}%.`}>
          <Slider
            value={volume}
            min={0}
            max={100}
            step={1}
            sound="volume"
            onChange={(v) => {
              setVolume(v);
              Music.setVolume(v);
            }}
            ariaLabel="Music volume"
          />
        </SettingRow>
      )}
    </SettingsSection>
  );
}
