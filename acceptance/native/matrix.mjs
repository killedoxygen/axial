// Every case requires installed, independently observed evidence on each host.
// Fixture tests and a successful process spawn cannot satisfy these cases.
export const NATIVE_CASES = Object.freeze([
  "clean-machine-install",
  "platform-publisher-trust",
  "installed-start",
  "authenticated-json",
  "scoped-media",
  "sse-reconnect",
  "navigation-csp",
  "native-dialogs",
  "window-chrome",
  "offline-vanilla-journey",
  "close-busy-refusal",
  "shutdown-process-tree",
  "profile-restart-persistence",
  "update-check-download-stage",
  "update-reject-corrupt",
  "update-reject-untrusted",
  "update-busy-refusal",
  "installed-update-restart",
  "update-interruption-recovery",
  "update-profile-preservation",
]);
