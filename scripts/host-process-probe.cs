using System;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;

namespace Axial.HostProbe
{
    public sealed class ProbeResult
    {
        public string State { get; set; }
        public bool Settled { get; set; }
        public int? ExitCode { get; set; }
        public string StandardOutput { get; set; }
        public string StandardError { get; set; }
    }

    internal sealed class CaptureResult
    {
        internal string Text;
        internal bool LimitExceeded;
        internal bool Failed;
    }

    internal sealed class Settlement
    {
        internal bool Settled;
        internal CaptureResult StandardOutput;
        internal CaptureResult StandardError;
    }

    public static class WindowsJobProcess
    {
        private const uint CREATE_NO_WINDOW = 0x08000000;
        private const uint CREATE_SUSPENDED = 0x00000004;
        private const uint EXTENDED_STARTUPINFO_PRESENT = 0x00080000;
        private const uint STARTF_USESTDHANDLES = 0x00000100;
        private const uint HANDLE_FLAG_INHERIT = 0x00000001;
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const int JobObjectBasicAccountingInformation = 1;
        private const int JobObjectExtendedLimitInformation = 9;
        private const int PROC_THREAD_ATTRIBUTE_HANDLE_LIST = 0x00020002;
        private const uint WAIT_OBJECT_0 = 0;
        private const uint WAIT_TIMEOUT = 258;
        private const uint OPEN_EXISTING = 3;
        private const uint GENERIC_READ = 0x80000000;
        private const uint FILE_SHARE_READ = 0x00000001;
        private const uint FILE_SHARE_WRITE = 0x00000002;
        private static readonly IntPtr InvalidHandle = new IntPtr(-1);

        [StructLayout(LayoutKind.Sequential)]
        private struct SECURITY_ATTRIBUTES
        {
            internal int nLength;
            internal IntPtr lpSecurityDescriptor;
            [MarshalAs(UnmanagedType.Bool)] internal bool bInheritHandle;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        private struct STARTUPINFO
        {
            internal int cb;
            internal string lpReserved;
            internal string lpDesktop;
            internal string lpTitle;
            internal uint dwX;
            internal uint dwY;
            internal uint dwXSize;
            internal uint dwYSize;
            internal uint dwXCountChars;
            internal uint dwYCountChars;
            internal uint dwFillAttribute;
            internal uint dwFlags;
            internal short wShowWindow;
            internal short cbReserved2;
            internal IntPtr lpReserved2;
            internal IntPtr hStdInput;
            internal IntPtr hStdOutput;
            internal IntPtr hStdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct STARTUPINFOEX
        {
            internal STARTUPINFO StartupInfo;
            internal IntPtr lpAttributeList;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct PROCESS_INFORMATION
        {
            internal IntPtr hProcess;
            internal IntPtr hThread;
            internal uint dwProcessId;
            internal uint dwThreadId;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_LIMIT_INFORMATION
        {
            internal long PerProcessUserTimeLimit;
            internal long PerJobUserTimeLimit;
            internal uint LimitFlags;
            internal UIntPtr MinimumWorkingSetSize;
            internal UIntPtr MaximumWorkingSetSize;
            internal uint ActiveProcessLimit;
            internal IntPtr Affinity;
            internal uint PriorityClass;
            internal uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS
        {
            internal ulong ReadOperationCount;
            internal ulong WriteOperationCount;
            internal ulong OtherOperationCount;
            internal ulong ReadTransferCount;
            internal ulong WriteTransferCount;
            internal ulong OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION
        {
            internal JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            internal IO_COUNTERS IoInfo;
            internal UIntPtr ProcessMemoryLimit;
            internal UIntPtr JobMemoryLimit;
            internal UIntPtr PeakProcessMemoryUsed;
            internal UIntPtr PeakJobMemoryUsed;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
        {
            internal long TotalUserTime;
            internal long TotalKernelTime;
            internal long ThisPeriodTotalUserTime;
            internal long ThisPeriodTotalKernelTime;
            internal uint TotalPageFaultCount;
            internal uint TotalProcesses;
            internal uint ActiveProcesses;
            internal uint TotalTerminatedProcesses;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateJobObjectW(IntPtr attributes, string name);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool SetInformationJobObject(
            IntPtr job,
            int informationClass,
            ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION information,
            uint informationLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool QueryInformationJobObject(
            IntPtr job,
            int informationClass,
            out JOBOBJECT_BASIC_ACCOUNTING_INFORMATION information,
            uint informationLength,
            IntPtr returnLength);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool TerminateJobObject(IntPtr job, uint exitCode);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreateProcessW(
            string applicationName,
            StringBuilder commandLine,
            IntPtr processAttributes,
            IntPtr threadAttributes,
            [MarshalAs(UnmanagedType.Bool)] bool inheritHandles,
            uint creationFlags,
            IntPtr environment,
            string currentDirectory,
            ref STARTUPINFOEX startupInfo,
            out PROCESS_INFORMATION processInformation);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreatePipe(
            out IntPtr readPipe,
            out IntPtr writePipe,
            ref SECURITY_ATTRIBUTES attributes,
            uint size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool SetHandleInformation(
            IntPtr handle,
            uint mask,
            uint flags);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateFileW(
            string fileName,
            uint desiredAccess,
            uint shareMode,
            ref SECURITY_ATTRIBUTES securityAttributes,
            uint creationDisposition,
            uint flagsAndAttributes,
            IntPtr templateFile);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool InitializeProcThreadAttributeList(
            IntPtr attributeList,
            int attributeCount,
            int flags,
            ref IntPtr size);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool UpdateProcThreadAttribute(
            IntPtr attributeList,
            uint flags,
            IntPtr attribute,
            IntPtr value,
            IntPtr size,
            IntPtr previousValue,
            IntPtr returnSize);

        [DllImport("kernel32.dll")]
        private static extern void DeleteProcThreadAttributeList(IntPtr attributeList);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint ResumeThread(IntPtr thread);

        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetExitCodeProcess(IntPtr process, out uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool TerminateProcess(IntPtr process, uint exitCode);

        private sealed class PipeEnds : IDisposable
        {
            internal SafeFileHandle Read;
            internal SafeFileHandle Write;

            internal static PipeEnds Create(SECURITY_ATTRIBUTES security)
            {
                IntPtr read;
                IntPtr write;
                if (!CreatePipe(out read, out write, ref security, 0)) {
                    throw new InvalidOperationException();
                }

                SafeFileHandle ownedRead = new SafeFileHandle(read, true);
                SafeFileHandle ownedWrite = new SafeFileHandle(write, true);
                try {
                    if (!SetHandleInformation(
                        ownedRead.DangerousGetHandle(),
                        HANDLE_FLAG_INHERIT,
                        0)) {
                        throw new InvalidOperationException();
                    }
                    return new PipeEnds { Read = ownedRead, Write = ownedWrite };
                }
                catch {
                    ownedRead.Dispose();
                    ownedWrite.Dispose();
                    throw;
                }
            }

            internal SafeFileHandle TakeRead()
            {
                SafeFileHandle value = Read;
                Read = null;
                return value;
            }

            internal void CloseWrite()
            {
                if (Write == null) return;
                Write.Dispose();
                Write = null;
            }

            public void Dispose()
            {
                if (Read != null) Read.Dispose();
                if (Write != null) Write.Dispose();
                Read = null;
                Write = null;
            }
        }

        private sealed class StartupAttributes : IDisposable
        {
            private IntPtr attributeList;
            private IntPtr handleValues;
            private bool initialized;

            internal StartupAttributes(IntPtr[] handles)
            {
                try {
                    IntPtr bytes = IntPtr.Zero;
                    InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref bytes);
                    if (bytes == IntPtr.Zero) throw new InvalidOperationException();

                    attributeList = Marshal.AllocHGlobal(bytes);
                    if (!InitializeProcThreadAttributeList(
                        attributeList,
                        1,
                        0,
                        ref bytes)) {
                        throw new InvalidOperationException();
                    }
                    initialized = true;

                    handleValues = Marshal.AllocHGlobal(
                        new IntPtr(checked(handles.Length * IntPtr.Size)));
                    for (int index = 0; index < handles.Length; index += 1) {
                        Marshal.WriteIntPtr(
                            handleValues,
                            index * IntPtr.Size,
                            handles[index]);
                    }
                    if (!UpdateProcThreadAttribute(
                        attributeList,
                        0,
                        new IntPtr(PROC_THREAD_ATTRIBUTE_HANDLE_LIST),
                        handleValues,
                        new IntPtr(handles.Length * IntPtr.Size),
                        IntPtr.Zero,
                        IntPtr.Zero)) {
                        throw new InvalidOperationException();
                    }
                }
                catch {
                    Dispose();
                    throw;
                }
            }

            internal IntPtr AttributeList
            {
                get { return attributeList; }
            }

            public void Dispose()
            {
                if (initialized) DeleteProcThreadAttributeList(attributeList);
                if (attributeList != IntPtr.Zero) Marshal.FreeHGlobal(attributeList);
                if (handleValues != IntPtr.Zero) Marshal.FreeHGlobal(handleValues);
                attributeList = IntPtr.Zero;
                handleValues = IntPtr.Zero;
                initialized = false;
            }
        }

        private sealed class ProbeOwnership : IDisposable
        {
            private readonly SafeFileHandle job;
            private SafeFileHandle process;
            private SafeFileHandle thread;
            private StreamReader outputReader;
            private StreamReader errorReader;
            private Task<CaptureResult> outputTask;
            private Task<CaptureResult> errorTask;
            private bool jobAssigned;
            private Settlement settlement;

            internal ProbeOwnership(SafeFileHandle jobHandle)
            {
                job = jobHandle;
            }

            internal IntPtr JobHandle
            {
                get { return job.DangerousGetHandle(); }
            }

            internal IntPtr ProcessHandle
            {
                get { return process.DangerousGetHandle(); }
            }

            internal IntPtr ThreadHandle
            {
                get { return thread.DangerousGetHandle(); }
            }

            internal void AttachProcess(
                SafeFileHandle processHandle,
                SafeFileHandle threadHandle)
            {
                process = processHandle;
                thread = threadHandle;
            }

            internal void AttachCaptures(
                SafeFileHandle outputRead,
                SafeFileHandle errorRead,
                int maximumCharacters)
            {
                StreamReader newOutput = null;
                StreamReader newError = null;
                try {
                    newOutput = Reader(outputRead);
                    outputRead = null;
                    newError = Reader(errorRead);
                    errorRead = null;
                    outputReader = newOutput;
                    errorReader = newError;
                    outputTask = StartCapture(outputReader, maximumCharacters);
                    errorTask = StartCapture(errorReader, maximumCharacters);
                }
                finally {
                    if (outputRead != null) outputRead.Dispose();
                    if (errorRead != null) errorRead.Dispose();
                    if (outputReader == null && newOutput != null) newOutput.Dispose();
                    if (errorReader == null && newError != null) newError.Dispose();
                }
            }

            internal void MarkJobAssigned()
            {
                jobAssigned = true;
            }

            internal void CloseThread()
            {
                if (thread == null) return;
                thread.Dispose();
                thread = null;
            }

            internal CaptureResult PreviewOutput()
            {
                return CompletedCapture(outputTask);
            }

            internal CaptureResult PreviewError()
            {
                return CompletedCapture(errorTask);
            }

            internal Settlement Settle()
            {
                if (settlement != null) return settlement;

                bool jobEmpty;
                bool rootExited = process == null;
                if (process == null) {
                    jobEmpty = WaitForEmptyJob(JobHandle, 2000);
                } else if (jobAssigned) {
                    jobEmpty = TerminateAndWaitForEmptyJob(JobHandle, 2000);
                    rootExited = WaitForSingleObject(ProcessHandle, 2000) == WAIT_OBJECT_0;
                } else {
                    rootExited = TerminateRootAndWait(ProcessHandle, 2000);
                    jobEmpty = WaitForEmptyJob(JobHandle, 2000);
                }

                bool capturesSettled = WaitForCaptures(outputTask, errorTask, 2000);
                settlement = new Settlement {
                    Settled = jobEmpty && rootExited && capturesSettled,
                    StandardOutput = CompletedCapture(outputTask),
                    StandardError = CompletedCapture(errorTask)
                };
                if (process == null) {
                    settlement.Settled = jobEmpty;
                }
                return settlement;
            }

            public void Dispose()
            {
                Settle();
                DisposeReaderAfterCapture(outputReader, outputTask);
                DisposeReaderAfterCapture(errorReader, errorTask);
                CloseThread();
                if (process != null) process.Dispose();
                job.Dispose();
                process = null;
                outputReader = null;
                errorReader = null;
                outputTask = null;
                errorTask = null;
            }
        }

        private static ProbeResult Result(
            string state,
            bool settled,
            int? exitCode,
            string standardOutput,
            string standardError)
        {
            return new ProbeResult {
                State = state,
                Settled = settled,
                ExitCode = exitCode,
                StandardOutput = standardOutput ?? String.Empty,
                StandardError = standardError ?? String.Empty
            };
        }

        private static StreamReader Reader(SafeFileHandle handle)
        {
            return new StreamReader(
                new FileStream(handle, FileAccess.Read, 256, false),
                new UTF8Encoding(false, false),
                true,
                256);
        }

        private static Task<CaptureResult> StartCapture(
            StreamReader reader,
            int maximumCharacters)
        {
            return Task.Factory.StartNew(
                () => ReadCapture(reader, maximumCharacters),
                CancellationToken.None,
                TaskCreationOptions.LongRunning,
                TaskScheduler.Default);
        }

        private static CaptureResult ReadCapture(
            StreamReader reader,
            int maximumCharacters)
        {
            StringBuilder value = new StringBuilder();
            char[] buffer = new char[256];
            try {
                while (true) {
                    int read = reader.Read(buffer, 0, buffer.Length);
                    if (read == 0) break;
                    if (value.Length + read > maximumCharacters) {
                        return new CaptureResult {
                            Text = value.ToString(),
                            LimitExceeded = true
                        };
                    }
                    value.Append(buffer, 0, read);
                }
                return new CaptureResult { Text = value.ToString() };
            }
            catch {
                return new CaptureResult { Text = value.ToString(), Failed = true };
            }
        }

        private static void DisposeReaderAfterCapture(
            StreamReader reader,
            Task<CaptureResult> task)
        {
            if (reader == null) return;
            if (task == null || task.IsCompleted) {
                reader.Dispose();
                return;
            }
            task.ContinueWith(
                ignored => reader.Dispose(),
                CancellationToken.None,
                TaskContinuationOptions.ExecuteSynchronously,
                TaskScheduler.Default);
        }

        private static CaptureResult CompletedCapture(Task<CaptureResult> task)
        {
            if (task == null || !task.IsCompleted || task.IsCanceled || task.IsFaulted) {
                return null;
            }
            return task.Result;
        }

        private static bool WaitForCaptures(
            Task<CaptureResult> output,
            Task<CaptureResult> error,
            int timeoutMilliseconds)
        {
            if (output == null || error == null) return false;
            try {
                return Task.WaitAll(
                    new Task[] { output, error },
                    timeoutMilliseconds);
            }
            catch {
                return false;
            }
        }

        private static string Quote(string value)
        {
            StringBuilder quoted = new StringBuilder(value.Length + 2);
            quoted.Append('"');
            int slashes = 0;
            foreach (char current in value) {
                if (current == '\\') {
                    slashes += 1;
                    continue;
                }
                if (current == '"') {
                    quoted.Append('\\', slashes * 2 + 1);
                    quoted.Append('"');
                    slashes = 0;
                    continue;
                }
                quoted.Append('\\', slashes);
                slashes = 0;
                quoted.Append(current);
            }
            quoted.Append('\\', slashes * 2);
            quoted.Append('"');
            return quoted.ToString();
        }

        private static bool TryGetActiveProcesses(
            IntPtr job,
            out uint activeProcesses)
        {
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION accounting;
            bool queried = QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                out accounting,
                (uint)Marshal.SizeOf(typeof(JOBOBJECT_BASIC_ACCOUNTING_INFORMATION)),
                IntPtr.Zero);
            activeProcesses = queried ? accounting.ActiveProcesses : UInt32.MaxValue;
            return queried;
        }

        private static bool WaitForEmptyJob(IntPtr job, int timeoutMilliseconds)
        {
            Stopwatch clock = Stopwatch.StartNew();
            while (true) {
                uint active;
                if (!TryGetActiveProcesses(job, out active)) return false;
                if (active == 0) return true;
                if (clock.ElapsedMilliseconds >= timeoutMilliseconds) return false;
                Thread.Sleep(10);
            }
        }

        private static bool TerminateAndWaitForEmptyJob(
            IntPtr job,
            int timeoutMilliseconds)
        {
            uint active;
            if (!TryGetActiveProcesses(job, out active)) return false;
            if (active > 0 && !TerminateJobObject(job, 1)) return false;
            return WaitForEmptyJob(job, timeoutMilliseconds);
        }

        private static bool TerminateRootAndWait(
            IntPtr process,
            int timeoutMilliseconds)
        {
            uint wait = WaitForSingleObject(process, 0);
            if (wait == WAIT_OBJECT_0) return true;
            if (wait != WAIT_TIMEOUT || !TerminateProcess(process, 1)) return false;
            return WaitForSingleObject(
                process,
                (uint)timeoutMilliseconds) == WAIT_OBJECT_0;
        }

        private static ProbeResult Complete(
            ProbeOwnership ownership,
            string state,
            int? exitCode,
            Stopwatch runtime,
            int timeoutMilliseconds)
        {
            Settlement settled = ownership.Settle();
            CaptureResult output = settled.StandardOutput;
            CaptureResult error = settled.StandardError;

            if ((output != null && output.LimitExceeded) ||
                (error != null && error.LimitExceeded)) {
                state = "output_limit_exceeded";
                exitCode = null;
            }
            if ((output != null && output.Failed) ||
                (error != null && error.Failed)) {
                state = "probe_failed";
                exitCode = null;
            }
            if (state == "completed" && runtime != null &&
                runtime.ElapsedMilliseconds >= timeoutMilliseconds) {
                state = "timed_out";
                exitCode = null;
            }
            if (!settled.Settled || output == null || error == null) {
                state = "probe_failed";
                exitCode = null;
            } else if (state != "completed") {
                exitCode = null;
            }

            return Result(
                state,
                settled.Settled,
                exitCode,
                output == null ? "" : output.Text,
                error == null ? "" : error.Text);
        }

        public static ProbeResult Run(
            string fileName,
            string arguments,
            int timeoutMilliseconds,
            int maximumCaptureCharacters)
        {
            if (String.IsNullOrWhiteSpace(fileName) || arguments == null ||
                timeoutMilliseconds < 50 || timeoutMilliseconds > 5000 ||
                maximumCaptureCharacters < 64 || maximumCaptureCharacters > 4096) {
                return Result("probe_failed", true, null, "", "");
            }

            SafeFileHandle job = null;
            SafeFileHandle nullInput = null;
            PipeEnds outputPipe = null;
            PipeEnds errorPipe = null;
            StartupAttributes attributes = null;
            ProbeOwnership ownership = null;
            Stopwatch runtime = null;

            try {
                IntPtr rawJob = CreateJobObjectW(IntPtr.Zero, null);
                if (rawJob == IntPtr.Zero) {
                    return Result("probe_failed", true, null, "", "");
                }
                job = new SafeFileHandle(rawJob, true);

                JOBOBJECT_EXTENDED_LIMIT_INFORMATION limits =
                    new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
                limits.BasicLimitInformation.LimitFlags =
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if (!SetInformationJobObject(
                    job.DangerousGetHandle(),
                    JobObjectExtendedLimitInformation,
                    ref limits,
                    (uint)Marshal.SizeOf(typeof(JOBOBJECT_EXTENDED_LIMIT_INFORMATION)))) {
                    return Result("probe_failed", true, null, "", "");
                }

                ownership = new ProbeOwnership(job);
                job = null;

                SECURITY_ATTRIBUTES security = new SECURITY_ATTRIBUTES {
                    nLength = Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)),
                    bInheritHandle = true
                };
                outputPipe = PipeEnds.Create(security);
                errorPipe = PipeEnds.Create(security);

                IntPtr rawNullInput = CreateFileW(
                    "NUL",
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ref security,
                    OPEN_EXISTING,
                    0,
                    IntPtr.Zero);
                if (rawNullInput == InvalidHandle) {
                    return Result("probe_failed", true, null, "", "");
                }
                nullInput = new SafeFileHandle(rawNullInput, true);

                IntPtr[] inheritedHandles = new IntPtr[] {
                    nullInput.DangerousGetHandle(),
                    outputPipe.Write.DangerousGetHandle(),
                    errorPipe.Write.DangerousGetHandle()
                };
                attributes = new StartupAttributes(inheritedHandles);

                STARTUPINFOEX startup = new STARTUPINFOEX();
                startup.StartupInfo.cb = Marshal.SizeOf(typeof(STARTUPINFOEX));
                startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
                startup.StartupInfo.hStdInput = inheritedHandles[0];
                startup.StartupInfo.hStdOutput = inheritedHandles[1];
                startup.StartupInfo.hStdError = inheritedHandles[2];
                startup.lpAttributeList = attributes.AttributeList;

                PROCESS_INFORMATION created;
                StringBuilder commandLine = new StringBuilder(
                    Quote(fileName) + (arguments.Length == 0 ? "" : " " + arguments));
                bool processCreated = CreateProcessW(
                    fileName,
                    commandLine,
                    IntPtr.Zero,
                    IntPtr.Zero,
                    true,
                    CREATE_SUSPENDED |
                        CREATE_NO_WINDOW |
                        EXTENDED_STARTUPINFO_PRESENT,
                    IntPtr.Zero,
                    null,
                    ref startup,
                    out created);

                outputPipe.CloseWrite();
                errorPipe.CloseWrite();
                nullInput.Dispose();
                nullInput = null;
                attributes.Dispose();
                attributes = null;

                if (!processCreated) {
                    return Result("probe_failed", true, null, "", "");
                }

                SafeFileHandle process = new SafeFileHandle(created.hProcess, true);
                SafeFileHandle thread = new SafeFileHandle(created.hThread, true);
                ownership.AttachProcess(process, thread);
                ownership.AttachCaptures(
                    outputPipe.TakeRead(),
                    errorPipe.TakeRead(),
                    maximumCaptureCharacters);

                if (!AssignProcessToJobObject(
                    ownership.JobHandle,
                    ownership.ProcessHandle)) {
                    return Complete(
                        ownership,
                        "probe_failed",
                        null,
                        null,
                        timeoutMilliseconds);
                }
                ownership.MarkJobAssigned();

                runtime = Stopwatch.StartNew();
                uint resumeResult = ResumeThread(ownership.ThreadHandle);
                ownership.CloseThread();
                if (resumeResult != 1) {
                    return Complete(
                        ownership,
                        "probe_failed",
                        null,
                        runtime,
                        timeoutMilliseconds);
                }

                string state = "completed";
                int? exitCode = null;
                while (true) {
                    if (runtime.ElapsedMilliseconds >= timeoutMilliseconds) {
                        state = "timed_out";
                        break;
                    }

                    CaptureResult output = ownership.PreviewOutput();
                    CaptureResult error = ownership.PreviewError();
                    if ((output != null && output.LimitExceeded) ||
                        (error != null && error.LimitExceeded)) {
                        state = "output_limit_exceeded";
                        break;
                    }
                    if ((output != null && output.Failed) ||
                        (error != null && error.Failed)) {
                        state = "probe_failed";
                        break;
                    }

                    uint wait = WaitForSingleObject(ownership.ProcessHandle, 0);
                    if (wait == WAIT_OBJECT_0) {
                        if (runtime.ElapsedMilliseconds >= timeoutMilliseconds) {
                            state = "timed_out";
                            break;
                        }
                        uint nativeExitCode;
                        if (!GetExitCodeProcess(
                            ownership.ProcessHandle,
                            out nativeExitCode)) {
                            state = "probe_failed";
                        } else {
                            exitCode = unchecked((int)nativeExitCode);
                        }
                        break;
                    }
                    if (wait != WAIT_TIMEOUT) {
                        state = "probe_failed";
                        break;
                    }
                    Thread.Sleep(10);
                }

                return Complete(
                    ownership,
                    state,
                    exitCode,
                    runtime,
                    timeoutMilliseconds);
            }
            catch {
                if (ownership == null) {
                    return Result("probe_failed", true, null, "", "");
                }
                return Complete(
                    ownership,
                    "probe_failed",
                    null,
                    runtime,
                    timeoutMilliseconds);
            }
            finally {
                if (attributes != null) attributes.Dispose();
                if (nullInput != null) nullInput.Dispose();
                if (outputPipe != null) outputPipe.Dispose();
                if (errorPipe != null) errorPipe.Dispose();
                if (ownership != null) ownership.Dispose();
                if (job != null) job.Dispose();
            }
        }
    }
}
