import { createHash } from "node:crypto";
import net from "node:net";
import { realpath } from "node:fs/promises";
import path from "node:path";

const PRIVATE_PORT_START = 49_152;
const PRIVATE_PORT_COUNT = 16_384;

export async function portablePathLeaseIdentity(candidate) {
  const resolved = path.resolve(candidate);
  const canonicalParent = await realpath(path.dirname(resolved));
  return path
    .normalize(path.join(canonicalParent, path.basename(resolved)))
    .toLowerCase();
}

export function privateLoopbackLeasePort(identity) {
  const digest = createHash("sha256").update(identity).digest();
  return PRIVATE_PORT_START + (digest.readUInt32BE(0) % PRIVATE_PORT_COUNT);
}

export async function acquireExclusiveLoopbackPort(port, options = {}) {
  const server = net.createServer((socket) => socket.destroy());
  if (options.unref === true) server.unref();

  await new Promise((resolve, reject) => {
    const onError = (error) => {
      server.removeListener("listening", onListening);
      reject(error);
    };
    const onListening = () => {
      server.removeListener("error", onError);
      resolve();
    };
    server.once("error", onError);
    server.once("listening", onListening);
    server.listen({ host: "127.0.0.1", port, exclusive: true });
  });

  let releasePromise;
  return () => {
    releasePromise ??= new Promise((resolve, reject) => {
      server.close((error) => {
        if (error) reject(error);
        else resolve();
      });
    });
    return releasePromise;
  };
}
