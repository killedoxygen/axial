const PREFERENCE_EXPORT_BYTES_LIMIT = 1024 * 1024;

export function exportBrowserPreferences(storage: { getItem(key: string): string | null }): string {
  let preferences: string | null;
  let route: string | null;
  try {
    preferences = storage.getItem('axial_ui');
    route = storage.getItem('axial:route');
  } catch {
    throw new Error('Stored browser preferences could not be read.');
  }

  const encoder = new TextEncoder();
  const tooLarge = (): never => {
    throw new Error('The browser preference export exceeds 1 MiB.');
  };
  if ((preferences?.length ?? 0) + (route?.length ?? 0) > PREFERENCE_EXPORT_BYTES_LIMIT) tooLarge();
  if (
    encoder.encode(preferences ?? '').byteLength + encoder.encode(route ?? '').byteLength >
    PREFERENCE_EXPORT_BYTES_LIMIT
  ) {
    tooLarge();
  }

  let parsedPreferences: unknown;
  let parsedRoute: unknown;
  try {
    parsedPreferences = preferences === null ? {} : JSON.parse(preferences);
    parsedRoute = route === null ? null : JSON.parse(route);
  } catch {
    throw new Error('Stored browser preferences contain invalid JSON.');
  }

  // Preserve unrecognized values for the importing application to validate.
  const exported = JSON.stringify(
    { format: 'axial-browser-preferences', version: 1, preferences: parsedPreferences, route: parsedRoute },
    null,
    2,
  );
  if (encoder.encode(exported).byteLength > PREFERENCE_EXPORT_BYTES_LIMIT) tooLarge();
  return exported;
}
