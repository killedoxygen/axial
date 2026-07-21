# Reports Windows host Java and Minecraft/Axial folder evidence as
# redacted key/value pairs. Never prints filesystem paths.
# Invoked by `task host:launch-evidence` on Windows and from WSL.

param(
  [switch]$SelfTest,
  [string]$JavaProbeFixture
)

$MaximumCount = 10000
$MaximumJavaCaptureCharacters = 4096
$JavaProbeTimeoutMilliseconds = 5000
$CurrentScriptPath = $PSCommandPath
$EvidenceKeys = @(
  'powershell',
  'windows_java_command',
  'windows_java_version',
  'windows_appdata_minecraft',
  'windows_minecraft_versions',
  'windows_minecraft_versions_count',
  'windows_minecraft_libraries',
  'windows_minecraft_assets',
  'windows_minecraft_runtime',
  'windows_store_runtime',
  'windows_appdata_axial',
  'windows_axial_instances',
  'windows_axial_instances_count',
  'windows_axial_library',
  'windows_axial_runtime',
  'windows_axial_runtime_count',
  'windows_paths_redacted'
)

if (-not [string]::IsNullOrEmpty($JavaProbeFixture)) {
  if (@('timeout', 'early_exit') -contains $JavaProbeFixture) {
    if ([string]::IsNullOrWhiteSpace($CurrentScriptPath) -or $CurrentScriptPath.Contains('"')) {
      exit 2
    }
    $childStart = [System.Diagnostics.ProcessStartInfo]::new()
    $childStart.FileName = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
    $childStart.Arguments = '-NoProfile -NonInteractive -File "{0}" -JavaProbeFixture child_wait' -f $CurrentScriptPath
    $childStart.UseShellExecute = $false
    $childStart.CreateNoWindow = $true
    $child = [System.Diagnostics.Process]::new()
    $child.StartInfo = $childStart
    try {
      if (-not $child.Start()) { exit 2 }
      [Console]::Out.WriteLine(('child_pid {0}' -f $child.Id))
      if ($JavaProbeFixture -eq 'timeout') {
        [System.Threading.Thread]::Sleep(10000)
      }
    }
    finally {
      $child.Dispose()
    }
    exit 0
  }

  switch ($JavaProbeFixture) {
    'success' {
      [Console]::Error.WriteLine('openjdk version "21.0.2+13-LTS" 2024-01-16 LTS')
      exit 0
    }
    'child_wait' {
      [System.Threading.Thread]::Sleep(10000)
      exit 0
    }
    'oversized' {
      [Console]::Out.Write(('x' * ($MaximumJavaCaptureCharacters * 2)))
      [System.Threading.Thread]::Sleep(10000)
      exit 0
    }
    'oversized_stderr' {
      [Console]::Error.Write(('x' * ($MaximumJavaCaptureCharacters * 2)))
      [System.Threading.Thread]::Sleep(10000)
      exit 0
    }
    'mixed' {
      [Console]::Out.WriteLine('stdout_marker')
      [Console]::Error.WriteLine('openjdk version "21.0.2+13-LTS" 2024-01-16 LTS')
      exit 0
    }
    default { exit 2 }
  }
}

function ConvertTo-SafeValue {
  param([AllowNull()][object]$Value)

  if ($null -eq $Value) {
    throw [System.IO.InvalidDataException]::new('unsafe evidence value')
  }

  $text = [System.Convert]::ToString($Value, [System.Globalization.CultureInfo]::InvariantCulture)
  if ($text -notmatch '\A[A-Za-z0-9._+-]{1,64}\z') {
    throw [System.IO.InvalidDataException]::new('unsafe evidence value')
  }
  return $text
}

function ConvertTo-EvidenceLines {
  param([System.Collections.IDictionary]$Evidence)

  if ($null -eq $Evidence -or $Evidence.Count -ne $EvidenceKeys.Count) {
    throw [System.IO.InvalidDataException]::new('invalid evidence contract')
  }
  $lines = @()
  foreach ($key in $EvidenceKeys) {
    if (-not $Evidence.Contains($key)) {
      throw [System.IO.InvalidDataException]::new('invalid evidence contract')
    }
    $value = ConvertTo-SafeValue $Evidence[$key]
    $lines += ('{0} {1}' -f $key, $value)
  }
  return $lines
}

function Join-ChildPath {
  param([AllowNull()][string]$Base, [string]$Name)

  if ([string]::IsNullOrWhiteSpace($Base)) {
    return $null
  }
  return [System.IO.Path]::Combine($Base, $Name)
}

function Measure-Location {
  param(
    [AllowNull()][string]$Path,
    [switch]$CountDirectories,
    [ValidateSet('actual', 'directory', 'file', 'missing', 'unknown', 'unauthorized', 'failure')]
    [string]$TestKind = 'actual',
    [ValidateSet('actual', 'zero', 'three', 'over_limit', 'unauthorized', 'failure')]
    [string]$TestCount = 'actual'
  )

  try {
    if ($TestKind -eq 'unauthorized') {
      throw [System.UnauthorizedAccessException]::new('self-test')
    }
    if ($TestKind -eq 'failure') {
      throw [System.InvalidOperationException]::new('self-test')
    }
    if ($TestKind -ne 'actual') {
      $state = $TestKind
    } elseif ([string]::IsNullOrWhiteSpace($Path)) {
      $state = 'unknown'
    } else {
      $items = @(Get-Item -LiteralPath $Path -Force -ErrorAction Stop)
      if ($items.Count -eq 0) {
        $state = 'missing'
      } elseif ($items.Count -ne 1) {
        $state = 'unknown'
      } else {
        $containerProperty = $items[0].PSObject.Properties['PSIsContainer']
        if ($null -eq $containerProperty) {
          $state = 'unknown'
        } elseif ([bool]$containerProperty.Value) {
          $state = 'directory'
        } else {
          $state = 'file'
        }
      }
    }
  }
  catch [System.UnauthorizedAccessException], [System.Security.SecurityException] { $state = 'inaccessible' }
  catch [System.Management.Automation.ItemNotFoundException], [System.Management.Automation.DriveNotFoundException], [System.IO.DirectoryNotFoundException], [System.IO.FileNotFoundException] { $state = 'missing' }
  catch { $state = 'probe_failed' }

  if (-not $CountDirectories) {
    return [pscustomobject]@{ State = $state; Count = $null }
  }
  if ($state -ne 'directory') {
    return [pscustomobject]@{ State = $state; Count = $state }
  }

  try {
    if ($TestCount -eq 'unauthorized') {
      throw [System.UnauthorizedAccessException]::new('self-test')
    }
    if ($TestCount -eq 'failure') {
      throw [System.InvalidOperationException]::new('self-test')
    }
    if ($TestCount -eq 'zero') {
      $count = 0
    } elseif ($TestCount -eq 'three') {
      $count = 3
    } elseif ($TestCount -eq 'over_limit') {
      $count = 'over_limit'
    } else {
      $items = @(
        Get-ChildItem -LiteralPath $Path -Directory -Force -ErrorAction Stop |
          Select-Object -First ($MaximumCount + 1) -ErrorAction Stop
      )
      if ($items.Count -gt $MaximumCount) {
        $count = 'over_limit'
      } else {
        $count = [int]$items.Count
      }
    }
  }
  catch [System.UnauthorizedAccessException], [System.Security.SecurityException] { $count = 'inaccessible' }
  catch { $count = 'probe_failed' }

  return [pscustomobject]@{ State = $state; Count = $count }
}

function ConvertTo-SafeJavaVersion {
  param([AllowNull()][object]$Line)

  if ($null -eq $Line) {
    return 'unknown'
  }
  $text = [string]$Line
  if ($text.Length -eq 0 -or $text.Length -gt 160) {
    return 'unknown'
  }

  $pattern = '\A(?:java|openjdk) version "([0-9][0-9A-Za-z]{0,15}(?:[._+-][0-9A-Za-z]{1,16}){0,8})"(?: [0-9]{4}-[0-9]{2}-[0-9]{2})?(?: LTS)?\z'
  $match = [regex]::Match($text, $pattern, [System.Text.RegularExpressions.RegexOptions]::CultureInvariant)
  if (-not $match.Success -or $match.Groups[1].Value.Length -gt 64) {
    return 'unknown'
  }
  return $match.Groups[1].Value
}

$WindowsJobProbeAvailable = $false
if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
  try {
    $probeSource = Join-Path $PSScriptRoot 'host-process-probe.cs'
    $probeSourceItem = Get-Item -LiteralPath $probeSource -Force -ErrorAction Stop
    if (
      $probeSourceItem.PSIsContainer -or
      ($probeSourceItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
      $probeSourceItem.Length -le 0 -or
      $probeSourceItem.Length -gt 131072
    ) {
      throw [System.IO.InvalidDataException]::new('invalid host probe source')
    }
    if ($null -eq ('Axial.HostProbe.WindowsJobProcess' -as [type])) {
      Add-Type -Path $probeSource -ErrorAction Stop
    }
    $WindowsJobProbeAvailable = $null -ne ('Axial.HostProbe.WindowsJobProcess' -as [type])
  }
  catch {
    $WindowsJobProbeAvailable = $false
  }
}
function New-ProcessResult {
  param(
    [string]$State,
    [bool]$Settled,
    [AllowNull()][object]$ExitCode,
    [string]$StandardOutput = '',
    [string]$StandardError = ''
  )

  return [pscustomobject]@{
    State = $State
    Settled = $Settled
    ExitCode = $ExitCode
    StandardOutput = $StandardOutput
    StandardError = $StandardError
  }
}

function Invoke-BoundedProcess {
  param(
    [string]$FileName,
    [string]$Arguments,
    [int]$TimeoutMilliseconds = $JavaProbeTimeoutMilliseconds,
    [int]$MaximumCaptureCharacters = $MaximumJavaCaptureCharacters
  )

  if (
    [string]::IsNullOrWhiteSpace($FileName) -or
    $TimeoutMilliseconds -lt 50 -or
    $TimeoutMilliseconds -gt $JavaProbeTimeoutMilliseconds -or
    $MaximumCaptureCharacters -lt 64 -or
    $MaximumCaptureCharacters -gt $MaximumJavaCaptureCharacters
  ) {
    return New-ProcessResult 'probe_failed' $true $null
  }
  if (-not $WindowsJobProbeAvailable) {
    return New-ProcessResult 'probe_failed' $true $null
  }
  try {
    return [Axial.HostProbe.WindowsJobProcess]::Run(
      $FileName,
      $Arguments,
      $TimeoutMilliseconds,
      $MaximumCaptureCharacters
    )
  }
  catch {
    return New-ProcessResult 'probe_failed' $false $null
  }
}

function ConvertFrom-JavaProcessResult {
  param([object]$Result)

  if (
    $null -eq $Result -or
    $Result.State -ne 'completed' -or
    $Result.Settled -ne $true -or
    $Result.ExitCode -ne 0
  ) {
    return 'probe_failed'
  }
  foreach ($capture in @($Result.StandardError, $Result.StandardOutput)) {
    foreach ($line in @($capture -split '\r?\n', 8)) {
      $version = ConvertTo-SafeJavaVersion $line
      if ($version -ne 'unknown') {
        return $version
      }
    }
  }
  return 'unknown'
}

function Get-JavaEvidence {
  try {
    $java = Get-Command java.exe -CommandType Application -TotalCount 1 -ErrorAction Stop
    if ($null -eq $java) {
      return [pscustomobject]@{ Command = 'missing'; Version = 'missing' }
    }
  }
  catch [System.Management.Automation.CommandNotFoundException] {
    return [pscustomobject]@{ Command = 'missing'; Version = 'missing' }
  }
  catch {
    return [pscustomobject]@{ Command = 'probe_failed'; Version = 'probe_failed' }
  }

  try {
    $result = Invoke-BoundedProcess $java.Source '-version'
    $version = ConvertFrom-JavaProcessResult $result
    return [pscustomobject]@{ Command = 'present'; Version = $version }
  }
  catch {
    return [pscustomobject]@{ Command = 'present'; Version = 'probe_failed' }
  }
}

function New-HostEvidence {
  $java = Get-JavaEvidence
  $minecraft = Join-ChildPath $env:APPDATA '.minecraft'
  $axial = Join-ChildPath $env:APPDATA 'axial'
  $axialLibrary = Join-ChildPath $axial 'library'

  $locations = @(
    @{ Key = 'windows_appdata_minecraft'; Path = $minecraft }
    @{ Key = 'windows_minecraft_versions'; Path = (Join-ChildPath $minecraft 'versions'); CountKey = 'windows_minecraft_versions_count' }
    @{ Key = 'windows_minecraft_libraries'; Path = (Join-ChildPath $minecraft 'libraries') }
    @{ Key = 'windows_minecraft_assets'; Path = (Join-ChildPath $minecraft 'assets') }
    @{ Key = 'windows_minecraft_runtime'; Path = (Join-ChildPath $minecraft 'runtime') }
    @{ Key = 'windows_store_runtime'; Path = (Join-ChildPath $env:LOCALAPPDATA 'Packages\Microsoft.4297127D64EC6_8wekyb3d8bbwe\LocalCache\Local\runtime') }
    @{ Key = 'windows_appdata_axial'; Path = $axial }
    @{ Key = 'windows_axial_instances'; Path = (Join-ChildPath $axial 'instances'); CountKey = 'windows_axial_instances_count' }
    @{ Key = 'windows_axial_library'; Path = $axialLibrary }
    @{ Key = 'windows_axial_runtime'; Path = (Join-ChildPath $axialLibrary 'runtime'); CountKey = 'windows_axial_runtime_count' }
  )

  $evidence = New-FailureEvidence
  $evidence.windows_java_command = $java.Command
  $evidence.windows_java_version = $java.Version
  foreach ($location in $locations) {
    $withCount = -not [string]::IsNullOrWhiteSpace($location.CountKey)
    $result = Measure-Location $location.Path -CountDirectories:$withCount
    $evidence[$location.Key] = $result.State
    if ($withCount) {
      $evidence[$location.CountKey] = $result.Count
    }
  }
  return $evidence
}

function New-FailureEvidence {
  $evidence = [ordered]@{}
  foreach ($key in $EvidenceKeys) {
    $evidence[$key] = 'probe_failed'
  }
  $evidence.powershell = 'yes'
  $evidence.windows_paths_redacted = 'yes'
  return $evidence
}

function Assert-Equal {
  param([AllowNull()][object]$Actual, [AllowNull()][object]$Expected)

  if ($Actual -ne $Expected) {
    throw [System.InvalidOperationException]::new('self-test assertion failed')
  }
}

function Assert-Fails {
  param([scriptblock]$Operation)

  try {
    & $Operation
  }
  catch {
    return
  }
  throw [System.InvalidOperationException]::new('self-test assertion failed')
}

function Assert-CapturedChildStopped {
  param([object]$Result)

  if ($Result.StandardOutput -notmatch '\Achild_pid ([0-9]{1,10})\r?\n?\z') {
    throw [System.InvalidOperationException]::new('self-test assertion failed')
  }
  $childStillRunning = $false
  try {
    $childProcess = [System.Diagnostics.Process]::GetProcessById([int]$Matches[1])
    try { $childStillRunning = -not $childProcess.HasExited } finally { $childProcess.Dispose() }
  }
  catch [System.ArgumentException] {}
  Assert-Equal $childStillRunning $false
}

function Invoke-JavaProbeFixture {
  param(
    [ValidateSet('success', 'timeout', 'early_exit', 'oversized', 'oversized_stderr', 'mixed')]
    [string]$Mode,
    [int]$TimeoutMilliseconds = 1000,
    [int]$MaximumCaptureCharacters = $MaximumJavaCaptureCharacters
  )

  if ([string]::IsNullOrWhiteSpace($CurrentScriptPath) -or $CurrentScriptPath.Contains('"')) {
    return New-ProcessResult 'probe_failed' $true $null
  }
  $executable = [System.Diagnostics.Process]::GetCurrentProcess().MainModule.FileName
  $arguments = '-NoProfile -NonInteractive -File "{0}" -JavaProbeFixture {1}' -f $CurrentScriptPath, $Mode
  return Invoke-BoundedProcess $executable $arguments $TimeoutMilliseconds $MaximumCaptureCharacters
}

function Invoke-SelfTest {
  foreach ($case in @(
    @{ Actual = (Measure-Location 'self-test' -TestKind directory).State; Expected = 'directory' }
    @{ Actual = (Measure-Location 'self-test' -TestKind missing).State; Expected = 'missing' }
    @{ Actual = (Measure-Location 'self-test' -TestKind unknown).State; Expected = 'unknown' }
    @{ Actual = (Measure-Location 'self-test' -TestKind unauthorized).State; Expected = 'inaccessible' }
    @{ Actual = (Measure-Location 'self-test' -TestKind failure).State; Expected = 'probe_failed' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind file).Count; Expected = 'file' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind missing).Count; Expected = 'missing' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind unknown).Count; Expected = 'unknown' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind directory -TestCount zero).Count; Expected = 0 }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind directory -TestCount three).Count; Expected = 3 }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind directory -TestCount over_limit).Count; Expected = 'over_limit' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind directory -TestCount unauthorized).Count; Expected = 'inaccessible' }
    @{ Actual = (Measure-Location 'self-test' -CountDirectories -TestKind directory -TestCount failure).Count; Expected = 'probe_failed' }
  )) {
    Assert-Equal $case.Actual $case.Expected
  }

  Assert-Equal (ConvertTo-SafeJavaVersion 'openjdk version "21.0.2+13-LTS" 2024-01-16 LTS') '21.0.2+13-LTS'
  Assert-Equal (ConvertTo-SafeJavaVersion 'java version "C:\private\java.exe"') 'unknown'
  Assert-Equal (ConvertTo-SafeJavaVersion (('x' * 161))) 'unknown'
  Assert-Equal (ConvertTo-SafeJavaVersion "openjdk version `"21.0.2`"`nprivate") 'unknown'

  if ([System.Environment]::OSVersion.Platform -eq [System.PlatformID]::Win32NT) {
    Assert-Equal $WindowsJobProbeAvailable $true

    $javaSuccess = Invoke-JavaProbeFixture success
    Assert-Equal $javaSuccess.State 'completed'
    Assert-Equal $javaSuccess.Settled $true
    Assert-Equal (ConvertFrom-JavaProcessResult $javaSuccess) '21.0.2+13-LTS'

    $javaTimeout = Invoke-JavaProbeFixture timeout -TimeoutMilliseconds 1000
    Assert-Equal $javaTimeout.State 'timed_out'
    Assert-Equal $javaTimeout.Settled $true
    Assert-Equal (ConvertFrom-JavaProcessResult $javaTimeout) 'probe_failed'
    Assert-CapturedChildStopped $javaTimeout

    $javaEarlyExit = Invoke-JavaProbeFixture early_exit
    Assert-Equal $javaEarlyExit.State 'completed'
    Assert-Equal $javaEarlyExit.Settled $true
    Assert-Equal $javaEarlyExit.ExitCode 0
    Assert-CapturedChildStopped $javaEarlyExit

    $javaOversized = Invoke-JavaProbeFixture oversized -MaximumCaptureCharacters 128
    Assert-Equal $javaOversized.State 'output_limit_exceeded'
    Assert-Equal $javaOversized.Settled $true
    Assert-Equal (ConvertFrom-JavaProcessResult $javaOversized) 'probe_failed'

    $javaOversizedError = Invoke-JavaProbeFixture oversized_stderr -MaximumCaptureCharacters 128
    Assert-Equal $javaOversizedError.State 'output_limit_exceeded'
    Assert-Equal $javaOversizedError.Settled $true
    Assert-Equal (ConvertFrom-JavaProcessResult $javaOversizedError) 'probe_failed'

    $javaMixed = Invoke-JavaProbeFixture mixed
    Assert-Equal $javaMixed.State 'completed'
    Assert-Equal $javaMixed.Settled $true
    Assert-Equal $javaMixed.StandardOutput.Replace("`r`n", "`n") "stdout_marker`n"
    Assert-Equal (ConvertFrom-JavaProcessResult $javaMixed) '21.0.2+13-LTS'
  } else {
    $unsupported = Invoke-JavaProbeFixture success
    Assert-Equal $unsupported.State 'probe_failed'
    Assert-Equal $unsupported.Settled $true
  }

  $fallback = New-FailureEvidence
  $null = @(ConvertTo-EvidenceLines $fallback)
  $fallback.windows_java_version = "private`npath"
  Assert-Fails { $null = @(ConvertTo-EvidenceLines $fallback) }
}

$failed = $false
$lines = @()
try {
  if ($SelfTest) {
    Invoke-SelfTest
  } else {
    $lines = @(ConvertTo-EvidenceLines (New-HostEvidence))
  }
}
catch {
  $failed = $true
  if (-not $SelfTest) {
    $lines = @(ConvertTo-EvidenceLines (New-FailureEvidence))
  }
}

if ($SelfTest) {
  if ($failed) {
    [Console]::Out.WriteLine('self_test failed')
    exit 1
  }
  [Console]::Out.WriteLine('self_test ok')
  exit 0
}

foreach ($line in $lines) {
  [Console]::Out.WriteLine($line)
}
