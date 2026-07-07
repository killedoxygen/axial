# Reports Windows host Java and Minecraft/Croopor folder evidence as
# redacted key/value pairs. Never prints filesystem paths.
# Invoked by `task host:launch-evidence` on Windows and from WSL.

$ErrorActionPreference = 'SilentlyContinue'

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
