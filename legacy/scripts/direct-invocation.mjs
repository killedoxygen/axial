import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

export function isDirectInvocation(moduleUrl, invokedPath = process.argv[1]) {
  if (
    typeof moduleUrl !== "string" ||
    typeof invokedPath !== "string" ||
    !invokedPath ||
    invokedPath.includes("\0")
  ) {
    return false;
  }

  try {
    return realpathSync(fileURLToPath(moduleUrl)) === realpathSync(invokedPath);
  } catch {
    return false;
  }
}
