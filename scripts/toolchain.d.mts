export type ToolchainManifest = {
  schema_version: 1;
  task: string;
  node: string;
  node_types: string;
  pnpm: string;
  rust: {
    release: string;
    rustc_commit: string;
    cargo_commit: string;
  };
  tauri_cli: string;
  cargo_deny: {
    release: string;
    linux_archive: {
      target: string;
      sha256: string;
    };
  };
  linux_ci_image: {
    reference: string;
    source_revision: string;
  };
  ubuntu_base: { reference: string };
  ubuntu_apt_snapshot: string;
};

export type ToolchainIdentity = ToolchainManifest & { manifest_sha256: string };

export function parseToolchainManifest(source: string): ToolchainManifest;
export function readToolchainIdentity(options?: {
  repositoryRoot?: string;
  manifestPath?: string;
}): ToolchainIdentity;
export function verifyToolchain(options?: {
  repositoryRoot?: string;
  identity?: ToolchainIdentity;
  profiles?: string[];
  runExecutable?: (command: string, args: string[]) => unknown;
}): unknown;
