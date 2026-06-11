import type { JSX } from 'preact';
import type { EnrichedInstance, InstanceResourceSummary } from '../../../types';
import { PerformanceCard } from './PerformanceCard';
import { WorldsCard } from './WorldsCard';
import { ActivityCard } from './ActivityCard';
import { QuickActionsCard } from './QuickActionsCard';
import { DetailsCard } from './DetailsCard';

export function OverviewPane({ inst, resources, running, onLaunch, onStop, onOpenWorlds, onOpenLogs, onRefreshResources }: {
  inst: EnrichedInstance;
  resources: InstanceResourceSummary | null;
  running: boolean;
  onLaunch: () => void;
  onStop: () => void;
  onOpenWorlds: () => void;
  onOpenLogs: () => void;
  onRefreshResources: () => void;
}): JSX.Element {
  return (
    <div class="cp-instance-body cp-instance-body--overview-bento">
      <div class="cp-od-slot cp-od-slot--worlds cp-od-worlds-slot">
        <WorldsCard inst={inst} resources={resources} onOpenWorlds={onOpenWorlds} onRefresh={onRefreshResources} />
      </div>
      <div class="cp-od-slot cp-od-slot--performance">
        <PerformanceCard inst={inst} />
      </div>
      <div class="cp-od-slot cp-od-slot--activity">
        <ActivityCard inst={inst} resources={resources} onOpenLogs={onOpenLogs} />
      </div>
      <div class="cp-od-slot cp-od-slot--quick">
        <QuickActionsCard
          inst={inst}
          running={running}
          onLaunch={onLaunch}
          onStop={onStop}
          onOpenLogs={onOpenLogs}
        />
      </div>
      <div class="cp-od-slot cp-od-slot--details">
        <DetailsCard inst={inst} running={running} />
      </div>
    </div>
  );
}
