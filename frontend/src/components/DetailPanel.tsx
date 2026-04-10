import type { JSX } from 'preact';
import { selectedInstance } from '../store';
import { InstanceDetail } from './InstanceDetail';

export function DetailPanel(): JSX.Element {
  const inst = selectedInstance.value;

  if (!inst) {
    return <></>;
  }

  return <InstanceDetail />;
}
