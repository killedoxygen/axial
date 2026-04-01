param(
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$Args
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $MyInvocation.MyCommand.Path
$ToolsDir = Join-Path $Root '.tools\bin'
$TaskExe = Join-Path $ToolsDir 'task.exe'
$TaskVersion = 'v3.49.1'

function Show-Help {
  @'
croopor dev cli

setup once
  .\dev.ps1 setup         install go deps, frontend deps, and local dev tools
  .\dev.cmd setup         same thing from cmd.exe

daily
  .\dev.ps1 dev           run desktop dev with wails
  .\dev.ps1 dev-web       run the frontend-only dev server
  .\dev.ps1 dev-windows   build and launch the windows dev binary
  .\dev.ps1 build         build the native release binary for this machine
  .\dev.ps1 build-dev     build the native dev binary for this machine
  .\dev.ps1 verify        run checks, tests, and native builds
  .\dev.ps1 clean         remove build outputs and go caches

other
  .\dev.ps1 build-windows
  .\dev.ps1 build-windows-dev
  .\dev.ps1 release-snapshot
  .\dev.ps1 doctor
  .\dev.ps1 help

notes
  - make is a unix/wsl convenience wrapper, not the windows entrypoint
  - direct task usage is optional, this script already routes to the repo-local task binary
'@ | Write-Host
}

function Ensure-Task {
  if (Test-Path $TaskExe) {
    return
  }

  New-Item -ItemType Directory -Force -Path $ToolsDir | Out-Null
  Write-Host 'bootstrapping local task into .tools\bin'
  $env:GOBIN = $ToolsDir
  go install "github.com/go-task/task/v3/cmd/task@$TaskVersion"
}

function Invoke-Interruptible {
  param(
    [Parameter(Mandatory = $true)]
    [string]$FilePath,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
  )

  & $FilePath @CommandArgs
  if ($LASTEXITCODE -eq 130) {
    return 0
  }
  return $LASTEXITCODE
}

function Invoke-InterruptibleInDir {
  param(
    [Parameter(Mandatory = $true)]
    [string]$Directory,
    [Parameter(Mandatory = $true)]
    [string]$FilePath,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
  )

  Push-Location $Directory
  try {
    return (Invoke-Interruptible $FilePath @CommandArgs)
  } finally {
    Pop-Location
  }
}

if ($Args.Count -eq 0) {
  Show-Help
  exit 0
}

$Command = $Args[0]
if ($Command -in @('help', '-h', '--help')) {
  Show-Help
  exit 0
}

if ($Command -eq 'bootstrap') {
  Ensure-Task
  exit 0
}

Ensure-Task
$env:PATH = "$ToolsDir;$env:PATH"

switch ($Command) {
  'dev' { exit (Invoke-Interruptible 'wails' 'dev') }
  'dev:desktop' { exit (Invoke-Interruptible 'wails' 'dev') }
  'dev-desktop' { exit (Invoke-Interruptible 'wails' 'dev') }
  'wails:dev' { exit (Invoke-Interruptible 'wails' 'dev') }
  'dev-web' { exit (Invoke-InterruptibleInDir (Join-Path $Root 'frontend') 'node' 'esbuild.mjs' 'serve') }
  'web' { exit (Invoke-InterruptibleInDir (Join-Path $Root 'frontend') 'node' 'esbuild.mjs' 'serve') }
  'watch' { exit (Invoke-InterruptibleInDir (Join-Path $Root 'frontend') 'node' 'esbuild.mjs' 'watch') }
  'frontend:serve' { exit (Invoke-InterruptibleInDir (Join-Path $Root 'frontend') 'node' 'esbuild.mjs' 'serve') }
  'frontend:watch' { exit (Invoke-InterruptibleInDir (Join-Path $Root 'frontend') 'node' 'esbuild.mjs' 'watch') }
}

& $TaskExe -d $Root @Args
exit $LASTEXITCODE
