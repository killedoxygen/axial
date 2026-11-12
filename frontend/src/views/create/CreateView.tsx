import type { JSX } from 'preact';
import { useEffect, useRef, useState } from 'preact/hooks';
import { Button, IconButton, Input, Kbd, Notice, Ring, type NoticeTone } from '../../ui/Atoms';
import { Face } from '../../ui/Face';
import { InstanceTile } from '../../ui/InstanceVisual';
import { Icon } from '../../ui/Icons';
import { Modal, ModalContent } from '../../ui/Modal';
import { SelectField } from '../../ui/Select';
import { closeCreate, createModpack, createOpen } from '../../ui-state';
import { createNoticePresentation } from '../../create-presenters';
import { LoaderTiles } from './LoaderTiles';
import { Options } from './Options';
import { StagedPicks } from './StagedPicks';
import { VersionPicker } from './VersionPicker';
import { useModpackFileImport } from './modpack-file';
import { useCreateForm, type CreateForm } from './use-create-form';
export { createBackendViewResponse, createLoaderBuildsResponse } from './contract';

export function CreateView(): JSX.Element {
  return (
    <Modal
      open={createOpen.value}
      onOpenChange={(next: boolean) => {
        if (!next) closeCreate();
      }}
    >
      <ModalContent
        className="cp-cr-card"
        aria-label="Create instance"
        aria-describedby={undefined}
        showCloseButton={false}
      >
        {/* An identified modpack file reopens the card in its setup mode. */}
        <CreateCard
          key={createModpack.value ? `${createModpack.value.canonical_id}:${createModpack.value.version_id}` : 'new'}
        />
      </ModalContent>
    </Modal>
  );
}

function LoaderBuild({ build }: { build: CreateForm['loaderBuild'] }): JSX.Element {
  if (build.loaded) {
    return (
      <div title={build.title}>
        <SelectField value={build.value} onChange={build.choose} options={build.options} ariaLabel="Loader version" />
      </div>
    );
  }
  if (build.error) return <span class="cp-cr-hint cp-cr-hint--err">{build.error}</span>;
  return (
    <span class="cp-cr-hint cp-cr-pending">
      <Ring size={14} />
      Loading…
    </span>
  );
}

function Notices({ form, packError }: { form: CreateForm; packError: string | null }): JSX.Element | null {
  const { pack, draft, staged, versions, notices } = form;
  const shown = [
    packError && <Notice key="pack" tone="error" role="alert" title={packError} />,
    draft && staged.error && (
      <Notice key="draft" tone="error" role="alert" title="Could not check what your picks support">
        <p>{staged.error}</p>
        {staged.retry && (
          <Button variant="secondary" size="sm" onClick={staged.retry}>
            Retry
          </Button>
        )}
      </Notice>
    ),
    !pack && draft && staged.candidates?.length === 0 && (
      <Notice key="common" tone="warned" role="alert" title="These picks share no common version">
        <p>Remove one of them and try again.</p>
      </Notice>
    ),
    !versions.loading && versions.error && (
      <Notice key="view" tone="error" role="alert" title="Couldn't load create options">
        <p>{versions.error}</p>
        <Button variant="secondary" size="sm" onClick={versions.reload}>
          Retry
        </Button>
      </Notice>
    ),
    ...notices.map((notice) => {
      const tone = createNoticePresentation(notice).tone as NoticeTone;
      return (
        <Notice
          key={notice.state_id}
          tone={tone}
          role={tone === 'warned' || tone === 'error' ? 'alert' : 'status'}
          title={notice.message}
        >
          {notice.detail && <p>{notice.detail}</p>}
        </Notice>
      );
    }),
  ].filter(Boolean);
  return shown.length > 0 ? <div class="cp-cr-notices">{shown}</div> : null;
}

function CreateCard(): JSX.Element {
  const form = useCreateForm();
  const { pack, draft, identity, versions } = form;
  const [versionsOpen, setVersionsOpen] = useState(false);
  const [optionsOpen, setOptionsOpen] = useState(false);
  const filterRef = useRef<HTMLInputElement | null>(null);
  const rootRef = useRef<HTMLDivElement | null>(null);
  const packFile = useModpackFileImport(rootRef, !pack);

  useEffect(() => {
    const handler = (e: KeyboardEvent): void => {
      if (form.submitting) return;
      const target = e.target as HTMLElement | null;
      const inField = target != null && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');
      if (e.key === 'Enter' && (e.ctrlKey || (!inField && target?.tagName !== 'BUTTON'))) {
        if (form.canCreate) {
          e.preventDefault();
          void form.submit();
        }
        return;
      }
      if (e.key === '/' && !inField && !pack) {
        e.preventDefault();
        setVersionsOpen(true);
        requestAnimationFrame(() => filterRef.current?.focus());
      }
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [form.canCreate, form.submitting, form.submit, pack]);

  return (
    <div class="cp-cr" ref={rootRef} data-drop={packFile.dropActive || undefined} {...packFile.dropHandlers}>
      {packFile.input}
      <header class="cp-cr-head">
        <h1>{pack ? 'Set up modpack' : 'New instance'}</h1>
        <IconButton icon="x" size={28} tooltip="Close (Esc)" onClick={closeCreate} />
      </header>

      <div class="cp-cr-form">
        <div class="cp-cr-art">
          <InstanceTile inst={identity.preview} />
          <button
            type="button"
            class="cp-cr-reroll"
            title="Shuffle colors"
            aria-label="Shuffle colors"
            onClick={identity.shuffleColor}
          >
            <Icon name="refresh" size={11} stroke={2.4} />
          </button>
        </div>
        <div class="cp-cr-name">
          <Input value={identity.name} onChange={identity.rename} placeholder={identity.placeholder} />
        </div>

        <span class="cp-cr-key">Loader</span>
        <LoaderTiles form={form} />

        <span class="cp-cr-key cp-cr-key--top">Minecraft</span>
        {pack ? (
          <div class="cp-select-trigger cp-cr-locked">
            <span class="cp-cr-mono">{pack.minecraft}</span>
          </div>
        ) : (
          <VersionPicker
            versions={versions}
            sourceKey={form.source.key}
            draft={draft}
            open={versionsOpen}
            onOpenChange={setVersionsOpen}
            filterRef={filterRef}
          />
        )}

        {form.loaderBuild.shown && (
          <>
            <span class="cp-cr-key">{form.source.label}</span>
            <LoaderBuild build={form.loaderBuild} />
          </>
        )}

        {draft && (
          <>
            <span class="cp-cr-key cp-cr-key--top">Installs after create</span>
            <StagedPicks draft={draft} staged={form.staged} />
          </>
        )}

        <Options options={form.options} open={optionsOpen} onOpenChange={setOptionsOpen} />
      </div>

      <Notices form={form} packError={packFile.error} />

      <footer class="cp-cr-foot">
        {!pack && !draft && (
          <Button
            variant="ghost"
            icon="folder"
            title="Set up an instance from a .mrpack file"
            busy={packFile.busy}
            disabled={packFile.busy}
            onClick={packFile.pick}
          >
            Import modpack file
          </Button>
        )}
        <span class="cp-head-fill" />
        <Button
          size="lg"
          busy={form.submitting}
          disabled={!form.canCreate}
          sound="affirm"
          trailing={<Kbd>↵</Kbd>}
          onClick={() => void form.submit()}
        >
          {pack ? 'Create and install' : 'Create instance'}
        </Button>
      </footer>

      {packFile.dropActive && (
        <div class="cp-cr-drop" aria-hidden="true">
          <Face size={48} />
          Drop to import this modpack
        </div>
      )}
    </div>
  );
}
