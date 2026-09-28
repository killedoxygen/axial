export interface LaunchLiveHandle {
  close(): void;
}

interface NativeLaunchTransportOptions {
  startPoll(handle: LaunchLiveHandle): LaunchLiveHandle;
  subscribeStatus(handle: LaunchLiveHandle): Promise<LaunchLiveHandle | null>;
  subscribeLog(): Promise<LaunchLiveHandle | null>;
  startBridge(): Promise<boolean>;
}

export async function establishNativeLaunchTransport({
  startPoll,
  subscribeStatus,
  subscribeLog,
  startBridge,
}: NativeLaunchTransportOptions): Promise<LaunchLiveHandle> {
  let status: LaunchLiveHandle | null = null;
  let log: LaunchLiveHandle | null = null;
  let poll: LaunchLiveHandle | null = null;

  const closeNative = (): void => {
    status?.close();
    log?.close();
    status = null;
    log = null;
  };
  const handle = {
    close(): void {
      closeNative();
      poll?.close();
      poll = null;
    },
  };
  poll = startPoll(handle);

  try {
    status = await subscribeStatus(handle);
    log = await subscribeLog();
    if (!status || !log || !(await startBridge())) closeNative();
  } catch {
    closeNative();
  }

  return handle;
}
