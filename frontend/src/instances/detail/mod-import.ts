import { errMessage, toast } from '../../app/toast';
import { plural } from '../../ui/format';
import { importMod } from '../api';

/** Add jars one request at a time so one refusal names its own file. Resolves
 * true when any file was added, so the caller can refresh its resource list. */
export async function importModFiles(instanceId: string, files: File[]): Promise<boolean> {
  const imported: string[] = [];
  const failures: string[] = [];
  for (const file of files) {
    try {
      imported.push(...(await importMod(instanceId, file)).imported);
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
