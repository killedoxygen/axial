param(
  [Parameter(ValueFromRemainingArguments = $true)]
  [string[]]$Args
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $MyInvocation.MyCommand.Path

function Show-Help {
  @'
croopor dev cli

setup once
  .\dev.ps1 setup         install frontend deps and prefetch rust deps
  .\dev.cmd setup         same thing from cmd.exe

daily
  .\dev.ps1 dev           run desktop dev with rust + tauri
  .\dev.ps1 dev-web       run the frontend-only dev server
  .\dev.ps1 watch         rebuild frontend assets on file changes
  .\dev.ps1 check         run static checks
  .\dev.ps1 test          run rust tests
  .\dev.ps1 verify        run checks, tests, and a release desktop build
  .\dev.ps1 clean         remove build outputs and caches
  .\dev.ps1 host:launch-evidence
                            report Windows Java and Minecraft/Croopor folder evidence

build
  .\dev.ps1 build         build the release desktop binary
  .\dev.ps1 build-dev     build the dev desktop binary
  .\dev.ps1 build:api     build the dev api binary
  .\dev.ps1 build:api:release

rust
  .\dev.ps1 rust:check
  .\dev.ps1 rust:clippy
  .\dev.ps1 rust:fmt
  .\dev.ps1 rust:fmt:fix
  .\dev.ps1 rust:test
  .\dev.ps1 rust:api
  .\dev.ps1 rust:desktop

frontend
  .\dev.ps1 frontend:install
  .\dev.ps1 frontend:check
  .\dev.ps1 frontend:build
  .\dev.ps1 frontend:watch
  .\dev.ps1 frontend:serve

other
  .\dev.ps1 doctor
  .\dev.ps1 help

notes
  - make is a unix/wsl convenience wrapper, not the windows entrypoint
  - host:launch-evidence reports redacted status/count evidence and does not print filesystem paths
  - Taskfile.yml mirrors these commands but is optional
'@ | Write-Host
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

function Invoke-Frontend {
  param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
  )

  Push-Location (Join-Path $Root 'frontend')
  try {
    & corepack pnpm @CommandArgs
    return $LASTEXITCODE
  } finally {
    Pop-Location
  }
}

function Run-Doctor {
  Write-Host ('rustc    ' + (rustc --version))
  Write-Host ('cargo    ' + (cargo --version))
  try { Write-Host ('rustfmt  ' + (cargo fmt --version)) } catch { Write-Host 'rustfmt  missing' }
  try { Write-Host ('clippy   ' + (cargo clippy --version)) } catch { Write-Host 'clippy   missing' }
  Write-Host ('node     ' + (node --version))
  Write-Host ('corepack ' + (corepack --version))
  try { Write-Host ('task     ' + (task --version)) } catch { Write-Host 'task     optional' }
  Write-Host 'wsl      no'
}

function Run-HostLaunchEvidence {
  function Emit {
    param([string]$Key, [object]$Value)
    if ($null -eq $Value -or [string]::IsNullOrWhiteSpace([string]$Value)) {
      $Value = 'unknown'
    }
    [Console]::Out.WriteLine(('{0} {1}' -f $Key, $Value))
  }

  function Child {
    param([string]$Base, [string]$Name)
    if ([string]::IsNullOrWhiteSpace($Base)) {
      return $null
    }
    return Join-Path $Base $Name
  }

  function LocationState {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
      return 'unknown'
    }
    if (Test-Path -LiteralPath $Path -PathType Container) {
      return 'present'
    }
    if (Test-Path -LiteralPath $Path -PathType Leaf) {
      return 'file'
    }
    return 'missing'
  }

  function DirectoryCount {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path -PathType Container)) {
      return 0
    }
    $items = @(Get-ChildItem -LiteralPath $Path -Directory -Force -ErrorAction SilentlyContinue)
    return $items.Count
  }

  Emit 'powershell' 'yes'

  $java = Get-Command java.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($java) {
    Emit 'windows_java_command' 'present'
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $versionLine = (& java.exe -version 2>&1 | ForEach-Object { [string]$_ } | Select-Object -First 1)
    $ErrorActionPreference = $oldErrorActionPreference
    if ($versionLine -match '"([^"]+)"') {
      Emit 'windows_java_version' $Matches[1]
    } else {
      Emit 'windows_java_version' 'unknown'
    }
  } else {
    Emit 'windows_java_command' 'missing'
    Emit 'windows_java_version' 'missing'
  }

  $minecraft = Child $env:APPDATA '.minecraft'
  $minecraftRuntime = Child $minecraft 'runtime'
  $minecraftVersions = Child $minecraft 'versions'
  $minecraftLibraries = Child $minecraft 'libraries'
  $minecraftAssets = Child $minecraft 'assets'
  $storeRuntime = Child $env:LOCALAPPDATA 'Packages\Microsoft.4297127D64EC6_8wekyb3d8bbwe\LocalCache\Local\runtime'

  $croopor = Child $env:APPDATA 'croopor'
  $crooporInstances = Child $croopor 'instances'
  $crooporLibrary = Child $croopor 'library'
  $crooporRuntime = Child $crooporLibrary 'runtime'

  Emit 'windows_appdata_minecraft' (LocationState $minecraft)
  Emit 'windows_minecraft_versions' (LocationState $minecraftVersions)
  Emit 'windows_minecraft_versions_count' (DirectoryCount $minecraftVersions)
  Emit 'windows_minecraft_libraries' (LocationState $minecraftLibraries)
  Emit 'windows_minecraft_assets' (LocationState $minecraftAssets)
  Emit 'windows_minecraft_runtime' (LocationState $minecraftRuntime)
  Emit 'windows_store_runtime' (LocationState $storeRuntime)

  Emit 'windows_appdata_croopor' (LocationState $croopor)
  Emit 'windows_croopor_instances' (LocationState $crooporInstances)
  Emit 'windows_croopor_instances_count' (DirectoryCount $crooporInstances)
  Emit 'windows_croopor_library' (LocationState $crooporLibrary)
  Emit 'windows_croopor_runtime' (LocationState $crooporRuntime)
  Emit 'windows_croopor_runtime_count' (DirectoryCount $crooporRuntime)
  Emit 'windows_paths_redacted' 'yes'
}

function Run-Check {
  Invoke-Frontend run check
  if ($LASTEXITCODE -ne 0) { return $LASTEXITCODE }
  cargo fmt --all --check
  if ($LASTEXITCODE -ne 0) { return $LASTEXITCODE }
  cargo check --workspace --locked
  if ($LASTEXITCODE -ne 0) { return $LASTEXITCODE }
  cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
  return $LASTEXITCODE
}

function Run-Verify {
  Invoke-Frontend run build
  if ($LASTEXITCODE -ne 0) { return $LASTEXITCODE }
  $status = Run-Check
  if ($status -ne 0) { return $status }
  cargo test --workspace --locked
  if ($LASTEXITCODE -ne 0) { return $LASTEXITCODE }
  cargo build --locked -p croopor-desktop --release
  return $LASTEXITCODE
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

switch ($Command) {
  'setup' {
    Invoke-Frontend install --frozen-lockfile --ignore-scripts
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo fetch --locked
    exit $LASTEXITCODE
  }
  'doctor' { Run-Doctor; exit 0 }
  'host:launch-evidence' { Run-HostLaunchEvidence; exit 0 }
  'dev' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'dev:desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'dev-desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'rust:desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'rust:api' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-api') }
  'dev-web' { Invoke-Frontend run dev; exit $LASTEXITCODE }
  'web' { Invoke-Frontend run dev; exit $LASTEXITCODE }
  'watch' { Invoke-Frontend run watch; exit $LASTEXITCODE }
  'frontend:serve' { Invoke-Frontend run dev; exit $LASTEXITCODE }
  'frontend:watch' { Invoke-Frontend run watch; exit $LASTEXITCODE }
  'frontend:install' { Invoke-Frontend install --frozen-lockfile --ignore-scripts; exit $LASTEXITCODE }
  'frontend:check' { Invoke-Frontend run check; exit $LASTEXITCODE }
  'frontend:build' { Invoke-Frontend run build; exit $LASTEXITCODE }
  'rust:fetch' { cargo fetch --locked; exit $LASTEXITCODE }
  'rust:fmt' { cargo fmt --all --check; exit $LASTEXITCODE }
  'rust:fmt:fix' { cargo fmt --all; exit $LASTEXITCODE }
  'rust:check' { cargo check --workspace --locked; exit $LASTEXITCODE }
  'rust:clippy' { cargo clippy --workspace --all-targets --all-features --locked -- -D warnings; exit $LASTEXITCODE }
  'rust:test' { cargo test --workspace --locked; exit $LASTEXITCODE }
  'test' { cargo test --workspace --locked; exit $LASTEXITCODE }
  'check' { exit (Run-Check) }
  'verify' { exit (Run-Verify) }
  'build' {
    Invoke-Frontend run build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --locked -p croopor-desktop --release
    exit $LASTEXITCODE
  }
  'build-dev' {
    Invoke-Frontend run build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --locked -p croopor-desktop
    exit $LASTEXITCODE
  }
  'build:api' {
    Invoke-Frontend run build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --locked -p croopor-api
    exit $LASTEXITCODE
  }
  'build:api:release' {
    Invoke-Frontend run build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --locked -p croopor-api --release
    exit $LASTEXITCODE
  }
  'clean' {
    cargo clean
    if (Test-Path (Join-Path $Root 'dist')) {
      Remove-Item -Recurse -Force (Join-Path $Root 'dist')
    }
    exit 0
  }
  default {
    Write-Error "unknown command: $Command`nrun '.\dev.ps1 help' to see available commands."
  }
}
