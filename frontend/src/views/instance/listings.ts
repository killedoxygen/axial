import { useEffect, useState } from 'preact/hooks';
import { contentRevision } from '../../content-activity';
import { errMessage } from '../../utils';
import { cachedModProvenance, fetchModProvenance, type ModProvenance } from './mod-actions';
import { lastListing, readListing } from './listing-cache';
import type { LoadState } from './resources';

/** A listing, read again when the instance's content changes or `refresh` is called. The last answer stays meanwhile. */
export function useListing<T>(
  instanceId: string,
  name: string,
  load: () => Promise<T>,
): LoadState<T> & { refresh: () => void } {
  const [stamp, setStamp] = useState(0);
  const [state, setState] = useState<LoadState<T>>(() => ({ status: 'loading', data: lastListing(instanceId, name) }));
  useEffect(() => {
    let alive = true;
    setState({ status: 'loading', data: lastListing(instanceId, name) });
    const keep = readListing(instanceId, name);
    load().then(
      (data) => {
        keep(data);
        if (alive) setState({ status: 'ready', data });
      },
      (err) => {
        if (alive) setState({ status: 'error', data: lastListing(instanceId, name), error: errMessage(err) });
      },
    );
    return () => {
      alive = false;
    };
  }, [instanceId, name, stamp, contentRevision.value]);
  return { ...state, refresh: () => setStamp((value) => value + 1) };
}

/** Where an instance's files came from and what is newer, read again with its content. */
export function useModProvenance(
  instanceId: string,
  stamp: number,
): { provenance: ModProvenance | null; error: string } {
  const [error, setError] = useState('');
  useEffect(() => {
    let alive = true;
    setError('');
    void fetchModProvenance(instanceId, () => {}).catch(() => {
      if (alive) setError('Could not load mod details. Refresh to try again.');
    });
    return () => {
      alive = false;
    };
  }, [instanceId, stamp, contentRevision.value]);
  return { provenance: cachedModProvenance(instanceId), error };
}
