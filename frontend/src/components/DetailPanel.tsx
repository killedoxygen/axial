import type { JSX } from 'preact';
import { selectedInstance } from '../store';
import { InstanceDetail } from './InstanceDetail';
import { ActionArea } from './ActionArea';

export function DetailPanel(): JSX.Element {
  const inst = selectedInstance.value;

  if (!inst) {
    return <></>;
  }

  return (
    <>
      <InstanceDetail />
      <div class="detail-actions">
        <ActionArea />
      </div>
    </>
  );
}
