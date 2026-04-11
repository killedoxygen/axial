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

  & corepack pnpm --dir (Join-Path $Root 'frontend') @CommandArgs
  return $LASTEXITCODE
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
  'dev' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'dev:desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'dev-desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'rust:desktop' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-desktop') }
  'rust:api' { exit (Invoke-Interruptible 'cargo' 'run' '--locked' '-p' 'croopor-api') }
  'dev-web' { exit (Invoke-Interruptible 'corepack' 'pnpm' '--dir' (Join-Path $Root 'frontend') 'run' 'dev') }
  'web' { exit (Invoke-Interruptible 'corepack' 'pnpm' '--dir' (Join-Path $Root 'frontend') 'run' 'dev') }
  'watch' { exit (Invoke-Interruptible 'corepack' 'pnpm' '--dir' (Join-Path $Root 'frontend') 'run' 'watch') }
  'frontend:serve' { exit (Invoke-Interruptible 'corepack' 'pnpm' '--dir' (Join-Path $Root 'frontend') 'run' 'dev') }
  'frontend:watch' { exit (Invoke-Interruptible 'corepack' 'pnpm' '--dir' (Join-Path $Root 'frontend') 'run' 'watch') }
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
  'build:api' { cargo build --locked -p croopor-api; exit $LASTEXITCODE }
  'build:api:release' { cargo build --locked -p croopor-api --release; exit $LASTEXITCODE }
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
