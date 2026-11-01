import { uploadFile } from '../../content-upload';
import { dtoArray, dtoRecord, dtoString } from '../../dto-contract';
import { plural } from '../../format';
import type { ModImportResponse } from '../../generated/ModImportResponse';
import { toast } from '../../toast';
import { errMessage } from '../../utils';

function modImportResponse(value: unknown): ModImportResponse {
  const record = dtoRecord(value, 'Mod import');
  return { imported: dtoArray(record.imported, 'Imported mods').map((name) => dtoString(name, 'Imported mod')) };
}

/** Add jars one request at a time so one refusal names its own file. Resolves
 * true when any file was added, so the caller can refresh its resource list. */
export async function importModFiles(instanceId: string, files: File[]): Promise<boolean> {
  const imported: string[] = [];
  const failures: string[] = [];
  for (const file of files) {
    const params = new URLSearchParams({ name: file.name });
    try {
      const response = modImportResponse(
        await uploadFile(`/instances/${encodeURIComponent(instanceId)}/mods/import?${params.toString()}`, file),
      );
      imported.push(...response.imported);
    } catch (error) {
      failures.push(errMessage(error));
    }
  }
  if (imported.length > 0) {
    toast(imported.length === 1 ? `Added ${imported[0]}` : `Added ${plural(imported.length, 'mod', 'mods')}`);
  }
  if (failures.length > 0) {
    toast(
      failures.length === 1
        ? failures[0]
        : `${failures[0]} ${plural(failures.length - 1, 'other file', 'other files')} also failed.`,
      'error',
    );
  }
  return imported.length > 0;
}
