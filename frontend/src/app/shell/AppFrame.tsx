import type { JSX, ComponentChildren } from 'preact';
import { Sidebar } from './Sidebar';
import { Topbar } from './Topbar';

export function AppFrame({ children }: { children: ComponentChildren }): JSX.Element {
  return (
    <div class="cp-frame">
      <Sidebar />
      <main class="cp-main">
        <Topbar />
        <div class="cp-view">{children}</div>
      </main>
    </div>
  );
}
