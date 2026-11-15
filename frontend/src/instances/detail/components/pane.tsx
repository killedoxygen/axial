import type { ComponentChildren, JSX, Ref } from 'preact';
import { IconButton } from '../../../ui/Atoms';
import type { LoadState } from '../resources';
import { PaneDoing, ResourceMutationStatus, ResourceStatus } from './resource-bits';

/**
 * What an instance tab shows: its toolbar on one line, what its listing says of itself, then its content. Every
 * tab is this frame, so they share one layout and nothing moves when the tab changes or its facts arrive.
 */
export function Pane({
  instanceId,
  bar,
  state,
  noun,
  onRetry,
  note,
  surface,
  children,
}: {
  instanceId: string;
  /** The toolbar. A pane with nothing in it has none. */
  bar?: JSX.Element | false;
  /** The listing behind the pane, while its wait or its refusal is worth a line. */
  state?: LoadState<unknown> | false;
  /** What the listing holds: "mods", "files". */
  noun?: string;
  onRetry: () => void;
  /** A refusal that is not the listing's own. */
  note?: string;
  /** The pane's own element, for a pane that takes dropped files. */
  surface?: JSX.HTMLAttributes<HTMLDivElement> & { ref?: Ref<HTMLDivElement>; 'data-drop-state'?: string };
  children?: ComponentChildren;
}): JSX.Element {
  return (
    <div class="cp-pane" {...surface}>
      {bar}
      {state && <ResourceStatus state={state} noun={noun} onRetry={onRetry} />}
      <ResourceMutationStatus instanceId={instanceId} refusalOnly />
      {note && (
        <div class="cp-resource-note cp-resource-note--error" role="alert">
          {note}
        </div>
      )}
      {children}
    </div>
  );
}

/**
 * A pane's toolbar: its own controls, then what is being done to the files in the free space, a fact about the
 * listing, and the tools every pane ends with (refresh, the folder, then its own).
 */
export function PaneBar({
  instanceId,
  children,
  fact,
  onRefresh,
  onOpenFolder,
  folderTip = 'Open folder',
  tools,
}: {
  instanceId: string;
  children?: ComponentChildren;
  fact?: ComponentChildren;
  onRefresh: () => void;
  onOpenFolder: () => void;
  folderTip?: string;
  tools?: ComponentChildren;
}): JSX.Element {
  return (
    <div class="cp-pane-bar">
      {children}
      <PaneDoing instanceId={instanceId} />
      {fact}
      <IconButton icon="refresh" tooltip="Refresh" onClick={() => onRefresh()} />
      <IconButton icon="folder" tooltip={folderTip} onClick={() => onOpenFolder()} />
      {tools}
    </div>
  );
}
