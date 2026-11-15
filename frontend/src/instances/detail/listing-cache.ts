/** What each of an instance's listings last answered, so a pane paints at once and refreshes underneath. */
interface Listing {
  data: unknown;
  /** The latest read to start. Only its answer is kept. */
  read: number;
}

const seen = new Map<string, Map<string, Listing>>();
let reads = 0;

export function lastListing<T>(instanceId: string, name: string): T | null {
  return (seen.get(instanceId)?.get(name)?.data as T | undefined) ?? null;
}

/** Starts a read. The returned function keeps its answer unless a later read began or the instance was forgotten. */
export function readListing(instanceId: string, name: string): (data: unknown) => void {
  let listings = seen.get(instanceId);
  if (!listings) seen.set(instanceId, (listings = new Map()));
  const read = ++reads;
  const listing = listings.get(name);
  if (listing) listing.read = read;
  else listings.set(name, { data: null, read });
  return (data) => {
    const current = seen.get(instanceId)?.get(name);
    if (current?.read === read) current.data = data;
  };
}

export function forgetListings(instanceId: string): void {
  seen.delete(instanceId);
}
