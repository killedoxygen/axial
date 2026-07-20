import process from "node:process";
import { pathToFileURL } from "node:url";

const [mode, modulePath] = process.argv.slice(2);
const maximumMessageBytes = 4 * 1024 * 1024;

class WorkerError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

function deepFreeze(value) {
  if (!value || typeof value !== "object" || Object.isFrozen(value)) {
    return value;
  }

  Object.freeze(value);
  for (const item of Object.values(value)) {
    deepFreeze(item);
  }
  return value;
}

function assertIpcSafe(value, seen = new Set(), depth = 0) {
  // The transport ceiling is deliberately wider than the evidence schema's
  // ceiling so the dispatcher remains the authority that classifies results.
  if (depth > 24) throw new Error("worker_result_too_deep");
  if (value === null || typeof value === "string" || typeof value === "boolean") return;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) throw new Error("worker_result_invalid_number");
    return;
  }
  if (typeof value !== "object" || seen.has(value)) throw new Error("worker_result_not_json");
  const prototype = Object.getPrototypeOf(value);
  if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) {
    throw new Error("worker_result_not_plain");
  }
  seen.add(value);
  for (const item of Array.isArray(value) ? value : Object.values(value)) {
    assertIpcSafe(item, seen, depth + 1);
  }
  seen.delete(value);
}

async function main() {
  if (!["inspect", "run", "receipts"].includes(mode) || !modulePath || !process.send) {
    throw new Error("invalid_worker_invocation");
  }

  let implementation;
  try {
    implementation = await import(pathToFileURL(modulePath).href);
  } catch {
    throw new WorkerError("implementation_load_failed");
  }
  const declaration = implementation.scenario;
  if (mode === "inspect") {
    return {
      ok: true,
      declaration,
      has_implementation: typeof implementation.runScenario === "function",
      has_receipt_revalidator: typeof implementation.readCurrentReceipts === "function",
    };
  }

  if (mode === "run" && typeof implementation.runScenario !== "function") {
    throw new Error("implementation_absent");
  }

  const context = JSON.parse(process.env.AXIAL_CAPABILITY_CONTEXT ?? "null");
  let result;
  const exit = process.exit;
  try {
    process.exit = () => {
      throw new WorkerError("scenario_failed");
    };
    result =
      mode === "receipts"
        ? await implementation.readCurrentReceipts?.(deepFreeze(context))
        : await implementation.runScenario(deepFreeze(context));
  } catch {
    throw new WorkerError("scenario_failed");
  } finally {
    process.exit = exit;
  }
  try {
    assertIpcSafe(result);
    if (Buffer.byteLength(JSON.stringify(result)) > maximumMessageBytes) {
      throw new Error("worker_result_too_large");
    }
  } catch {
    throw new WorkerError("malformed_scenario_result");
  }
  return { ok: true, result };
}

async function reportAndWait(message) {
  if (!process.send) process.exit(1);
  await new Promise((resolve, reject) => {
    process.send(message, (error) => (error ? reject(error) : resolve()));
  });
  // The dispatcher owns process-tree settlement. Keeping this leader alive lets
  // Windows target descendants by parent PID and POSIX target the process group.
  await new Promise(() => {});
}

main()
  .then((message) => reportAndWait(message))
  .catch((error) => {
    const code = error instanceof WorkerError ? error.code : "worker_failed";
    return reportAndWait({ ok: false, code });
  })
  .catch(() => process.exit(1));
