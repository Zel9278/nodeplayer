# Requires PowerShell 7 and Windows. No process-name kill, no user-profile overrides.
[CmdletBinding()]
param(
    [Parameter(Mandatory,Position=0)][string]$Executable,
    [Parameter(Mandatory,Position=1)][string]$Scenario,
    [Parameter(Position=2)][string]$RunDirectory,
    [Parameter(Position=3)][ValidateRange(0.001,86400)][double]$DurationSeconds = 30,
    [Parameter(ValueFromRemainingArguments)][string[]]$ExtraArguments = @()
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Use run-diagnostics.sh on non-Windows systems.' }
if (-not $RunDirectory) { $RunDirectory = Join-Path 'diagnostics-runs' ('run-{0}-{1}' -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss.fffffffZ'), $PID) }
$root = [IO.Path]::GetFullPath($RunDirectory)
$exe = (Resolve-Path -LiteralPath $Executable).Path
$scenarioPath = (Resolve-Path -LiteralPath $Scenario).Path
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($root)) | Out-Null
# Atomic directory reservation, unlike Directory.CreateDirectory which accepts an existing path.
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
using System.ComponentModel;
public sealed class DiagnosticJob : IDisposable {
    [StructLayout(LayoutKind.Sequential)] struct SA { public int nLength; public IntPtr descriptor; public int inherit; }
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct SI {
        public int cb; public string reserved, desktop, title; public int x,y,xs,ys,xc,yc,fill,flags; public short show, reserved2; public IntPtr reservedPtr, input, output, error;
    }
    [StructLayout(LayoutKind.Sequential)] struct SIX { public SI startup; public IntPtr attributes; }
    [StructLayout(LayoutKind.Sequential)] struct PI { public IntPtr process, thread; public uint pid, tid; }
    [StructLayout(LayoutKind.Sequential)] struct BASIC { public long processTime,jobTime; public uint flags; public UIntPtr min,max; public uint active; public UIntPtr affinity; public uint priority,scheduling; }
    [StructLayout(LayoutKind.Sequential)] struct IO { public ulong r,w,o,rb,wb,ob; }
    [StructLayout(LayoutKind.Sequential)] struct LIMIT { public BASIC basic; public IO io; public UIntPtr processMemory,jobMemory,peakProcess,peakJob; }
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateDirectoryW(string path,IntPtr security);
    [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr CreateJobObjectW(IntPtr security,string name);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job,int kind,ref LIMIT limit,int length);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job,IntPtr process);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessW(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr environment,string cwd,ref SIX si,out PI pi);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateFileW(string path,uint access,uint share,ref SA security,uint disposition,uint flags,IntPtr template);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool InitializeProcThreadAttributeList(IntPtr list,int count,int flags,ref IntPtr size);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool UpdateProcThreadAttribute(IntPtr list,uint flags,IntPtr key,IntPtr value,IntPtr size,IntPtr previous,IntPtr returned);
    [DllImport("kernel32.dll")] static extern void DeleteProcThreadAttributeList(IntPtr list);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr process,uint code);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out uint code);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint ms);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    IntPtr job, process; public uint Pid {get;private set;}
    static void Check(bool ok) { if(!ok) throw new Win32Exception(Marshal.GetLastWin32Error()); }
    public static void Reserve(string root) { Check(CreateDirectoryW(root,IntPtr.Zero)); }
    // CommandLineToArgvW-compatible quoting; never invoke cmd.exe or concatenate shell code.
    static string Quote(string s) {
        var b = new StringBuilder("\""); int slashes=0;
        foreach(char c in s) { if(c=='\\') {slashes++;continue;} if(c=='"') {b.Append('\\',slashes*2+1);b.Append(c);} else {b.Append('\\',slashes);b.Append(c);} slashes=0; }
        b.Append('\\',slashes*2); b.Append('"'); return b.ToString();
    }
    public DiagnosticJob(string exe,string[] args,string root) {
        IntPtr input=IntPtr.Zero,output=IntPtr.Zero,error=IntPtr.Zero,list=IntPtr.Zero,handles=IntPtr.Zero; bool initialized=false; PI pi=new PI();
        try {
            job=CreateJobObjectW(IntPtr.Zero,null); Check(job!=IntPtr.Zero);
            var limit=new LIMIT(); limit.basic.flags=0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            Check(SetInformationJobObject(job,9,ref limit,Marshal.SizeOf<LIMIT>()));
            var sa=new SA { nLength=Marshal.SizeOf<SA>(),inherit=1 };
            input=CreateFileW("NUL",0x80000000,3,ref sa,3,0,IntPtr.Zero);
            output=CreateFileW(System.IO.Path.Combine(root,"stdout.log"),0x40000000,3,ref sa,1,0,IntPtr.Zero);
            error=CreateFileW(System.IO.Path.Combine(root,"stderr.log"),0x40000000,3,ref sa,1,0,IntPtr.Zero);
            Check(input!=new IntPtr(-1)&&output!=new IntPtr(-1)&&error!=new IntPtr(-1));
            IntPtr bytes=IntPtr.Zero; InitializeProcThreadAttributeList(IntPtr.Zero,1,0,ref bytes);
            list=Marshal.AllocHGlobal(bytes); Check(InitializeProcThreadAttributeList(list,1,0,ref bytes)); initialized=true;
            handles=Marshal.AllocHGlobal(IntPtr.Size*3); Marshal.WriteIntPtr(handles,0,input); Marshal.WriteIntPtr(handles,IntPtr.Size,output); Marshal.WriteIntPtr(handles,IntPtr.Size*2,error);
            Check(UpdateProcThreadAttribute(list,0,new IntPtr(0x20002),handles,new IntPtr(IntPtr.Size*3),IntPtr.Zero,IntPtr.Zero));
            var si=new SIX(); si.startup.cb=Marshal.SizeOf<SIX>(); si.startup.flags=0x100; si.startup.input=input;si.startup.output=output;si.startup.error=error;si.attributes=list;
            var cmd=new StringBuilder(Quote(exe)); foreach(var arg in args) cmd.Append(' ').Append(Quote(arg));
            // Assign before executing any app/plugin code; no descendant can escape the job race.
            Check(CreateProcessW(exe,cmd,IntPtr.Zero,IntPtr.Zero,true,0x00080004,IntPtr.Zero,null,ref si,out pi));
            process=pi.process; Pid=pi.pid;
            Check(AssignProcessToJobObject(job,process));
            Check(ResumeThread(pi.thread)!=0xffffffff);
        } catch { if(process!=IntPtr.Zero) TerminateProcess(process,125); Dispose(); throw; }
        finally {
            if(pi.thread!=IntPtr.Zero) CloseHandle(pi.thread);
            foreach(var h in new[]{input,output,error}) if(h!=IntPtr.Zero&&h!=new IntPtr(-1)) CloseHandle(h);
            if(initialized) DeleteProcThreadAttributeList(list); if(list!=IntPtr.Zero) Marshal.FreeHGlobal(list); if(handles!=IntPtr.Zero) Marshal.FreeHGlobal(handles);
        }
    }
    public bool Exited { get { return WaitForSingleObject(process,0)==0; } }
    public uint ExitCode { get { uint code;Check(GetExitCodeProcess(process,out code));return code; } }
    public void Dispose() {
        if(job!=IntPtr.Zero) { CloseHandle(job);job=IntPtr.Zero; }
        if(process!=IntPtr.Zero) { WaitForSingleObject(process,5000);CloseHandle(process);process=IntPtr.Zero; }
    }
}
'@
[DiagnosticJob]::Reserve($root)
$artifacts = Join-Path $root 'artifacts'
$arguments = @('--diagnostic-scenario',$scenarioPath,'--diagnostic-output',$artifacts,'--diagnostic-duration',$DurationSeconds.ToString([Globalization.CultureInfo]::InvariantCulture)) + $ExtraArguments
$child = $null
$outcome = 'aborted'; $code = 125; $detail = 'launcher failed'; $childPid = $null
try {
    $child = [DiagnosticJob]::new($exe, $arguments, $root)
    $childPid = $child.Pid
    @{ pid=$childPid; watchdog_seconds=($DurationSeconds+10); tree_scope='owned Windows Job Object; kill-on-job-close'; no_breakaway=$true } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $root 'process.json') -Encoding utf8
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while (-not $child.Exited) {
        if ($clock.Elapsed.TotalSeconds -ge ($DurationSeconds + 10)) { $outcome='timeout';$code=124;$detail='external watchdog expired';break }
        if (@('stdout.log','stderr.log') | Where-Object { (Get-Item -LiteralPath (Join-Path $root $_)).Length -gt 32MB }) { $outcome='failed';$code=2;$detail='stdout/stderr exceeded 32 MiB watchdog threshold';break }
        Start-Sleep -Milliseconds 100
    }
    if ($child.Exited) {
        $native = $child.ExitCode
        $summary = Join-Path $artifacts 'summary.json'
        if (Test-Path -LiteralPath $summary) {
            $result = Get-Content -Raw -LiteralPath $summary | ConvertFrom-Json
            $expectedCodes = @{passed=0;failed=2;timeout=124;aborted=125}
            if ($null -eq $result.exit_code -or $null -eq $result.outcome -or -not $expectedCodes.ContainsKey([string]$result.outcome) -or $expectedCodes[[string]$result.outcome] -ne [int]$result.exit_code) { throw 'Invalid application summary' }
            $outcome=$result.outcome; $code=[int]$result.exit_code; $detail='application summary'
            if ($native -ne 0 -and $code -eq 0) { $outcome='crash';$code=3;$detail='nonzero process exit despite passing summary' }
        } else { $outcome=if($native -ne 0){'crash'}else{'failed'};$code=if($native -ne 0){3}else{2};$detail='process exited without summary' }
    }
} catch { $outcome='aborted';$code=125;$detail='launcher or artifact error'; Write-Warning $_ }
finally {
    if ($null -ne $child) { $child.Dispose() } # Kills only this job and waits for its parent.
    $result = @{outcome=$outcome;exit_code=$code;detail=$detail;artifacts=$artifacts;pid=$childPid;no_upload=$true}
    $result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $root 'runner-summary.json') -Encoding utf8
    Write-Host ($result | ConvertTo-Json -Compress)
}
exit $code
