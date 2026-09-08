param(
    [Parameter(Mandatory = $true)][string]$EntryScript
)
$ErrorActionPreference = 'Stop'
$entryPath = (Resolve-Path -LiteralPath $EntryScript).Path
$nodePath = (Get-Command node -CommandType Application | Select-Object -First 1).Source

# No SwitchDesktop call: the user's input desktop stays selected. Every child
# process inherits this private desktop, including native app restarts.
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class StartupPrivateDesktop {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct StartupInfo {
        public int cb; public string reserved; public string desktop; public string title;
        public uint x, y, width, height, xChars, yChars, fill, flags;
        public ushort show, reservedBytes;
        public IntPtr reservedPointer, stdin, stdout, stderr;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ProcessInfo { public IntPtr process, thread; public uint pid, tid; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateDesktopW(string name, IntPtr device, IntPtr mode, uint flags, uint access, IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool CreateProcessW(string app, StringBuilder command, IntPtr processSecurity, IntPtr threadSecurity,
        bool inherit, uint flags, IntPtr environment, string directory, ref StartupInfo startup, out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle, out uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);

    public static int Run(string node, string script, string directory) {
        if (node.Contains("\"") || script.Contains("\"")) throw new ArgumentException("Invalid executable path");
        string name = "risunest-startup-" + Guid.NewGuid().ToString("N");
        IntPtr desktop = CreateDesktopW(name, IntPtr.Zero, IntPtr.Zero, 0, 0x01ff, IntPtr.Zero);
        if (desktop == IntPtr.Zero) throw new InvalidOperationException("Private desktop creation failed: " + Marshal.GetLastWin32Error());
        ProcessInfo child = new ProcessInfo();
        try {
            StartupInfo startup = new StartupInfo();
            startup.cb = Marshal.SizeOf(startup); startup.desktop = name;
            startup.flags = 0x80; // STARTF_FORCEOFFFEEDBACK
            var command = new StringBuilder("\"" + node + "\" \"" + script + "\"");
            if (!CreateProcessW(node, command, IntPtr.Zero, IntPtr.Zero, false, 0x08000000,
                IntPtr.Zero, directory, ref startup, out child))
                throw new InvalidOperationException("Private desktop process creation failed: " + Marshal.GetLastWin32Error());
            WaitForSingleObject(child.process, 0xffffffff);
            uint code;
            if (!GetExitCodeProcess(child.process, out code)) return 1;
            return (int)code;
        } finally {
            if (child.thread != IntPtr.Zero) CloseHandle(child.thread);
            if (child.process != IntPtr.Zero) CloseHandle(child.process);
            CloseDesktop(desktop);
        }
    }
}
'@
$env:STARTUP_BENCHMARK_DISPLAY = 'private-desktop'
exit [StartupPrivateDesktop]::Run($nodePath, $entryPath, (Get-Location).Path)
