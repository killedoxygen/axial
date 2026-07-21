import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) =>
  readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

const taskBody = (taskfile, name) => {
  const marker = `\n  ${name}:\n`;
  const start = taskfile.indexOf(marker);
  assert.notEqual(start, -1, `missing task ${name}`);
  const remainder = taskfile.slice(start + marker.length);
  const next = remainder.search(/\n  [A-Za-z0-9][^ \n]*:\n/);
  return next === -1 ? remainder : remainder.slice(0, next);
};

test("P01-B01 keeps one explicit application-root authority", async () => {
  const [
    workspaceManifest,
    apiManifest,
    bootstrap,
    paths,
    runtimeLayout,
    state,
    instanceRegistry,
    accounts,
    skins,
    failureMemory,
    journals,
    knownGood,
    rejectionStreaks,
    performanceOperations,
    benchmarkSuites,
    benchmarkSuiteDrivers,
    launchReports,
    userModWitness,
    updater,
    performanceManager,
    performanceRulesCache,
    performanceRulesState,
    skinCache,
    desktopCommands,
    desktopBuild,
    flagsRoute,
    apiMain,
    desktopMain,
    taskfile,
    productionTauri,
    developmentTauri,
  ] = await Promise.all([
    read("Cargo.toml"),
    read("apps/api/Cargo.toml"),
    read("apps/api/src/bootstrap.rs"),
    read("core/config/src/paths/mod.rs"),
    read("core/minecraft/src/runtime/layout.rs"),
    read("apps/api/src/state/mod.rs"),
    read("apps/api/src/state/instance_registry.rs"),
    read("apps/api/src/state/accounts.rs"),
    read("apps/api/src/state/skins.rs"),
    read("apps/api/src/state/failure_memory.rs"),
    read("apps/api/src/state/journals.rs"),
    read("apps/api/src/state/known_good.rs"),
    read("apps/api/src/state/persisted_state_rejection_streaks.rs"),
    read("apps/api/src/state/performance_operations.rs"),
    read("apps/api/src/state/benchmark_suites.rs"),
    read("apps/api/src/state/benchmark_suite_drivers.rs"),
    read("apps/api/src/state/launch_reports.rs"),
    read("apps/api/src/state/user_mod_witness.rs"),
    read("apps/api/src/state/updater.rs"),
    read("core/performance/src/install/manager.rs"),
    read("core/performance/src/rules_cache.rs"),
    read("apps/api/src/state/performance_rules.rs"),
    read("apps/api/src/application/skin/cache.rs"),
    read("apps/desktop/src/commands/mod.rs"),
    read("apps/desktop/build.rs"),
    read("apps/api/src/routes/flags.rs"),
    read("apps/api/src/main.rs"),
    read("apps/desktop/src/main.rs"),
    read("Taskfile.yml"),
    read("apps/desktop/tauri.conf.json"),
    read("apps/desktop/tauri.dev.conf.json"),
  ]);

  assert.match(workspaceManifest, /^dirs = "=6\.0\.0"$/m);
  assert.match(apiManifest, /^dirs\.workspace = true$/m);
  assert.match(
    bootstrap,
    /pub const APP_IDENTIFIER: &str = "dev\.mateoltd\.axial";/,
  );
  assert.match(
    bootstrap,
    /pub const DEVELOPMENT_APP_IDENTIFIER: &str = "dev\.mateoltd\.axial\.dev";/,
  );
  assert.match(
    bootstrap,
    /pub fn desktop_app_root_selection_from_environment\(\s*native_identifier: &str,/,
  );
  assert.match(
    bootstrap,
    /APP_IDENTIFIER => Ok\(AppRootSelection::Production\)/,
  );
  assert.match(
    bootstrap,
    /DEVELOPMENT_APP_IDENTIFIER => Ok\(AppRootSelection::Development\)/,
  );
  assert.match(bootstrap, /Err\(AppRootError::NativeIdentifierMismatch\)/);
  assert.match(paths, /pub fn from_root\(/);
  assert.doesNotMatch(paths, /pub fn detect\(/);
  assert.doesNotMatch(paths, /pub fn root\s*\(/);
  assert.doesNotMatch(paths, /pub fn config_dir\(/);
  assert.doesNotMatch(
    paths,
    /pub (?:root|config_file|instances_file|instances_dir|music_dir|library_dir|runtimes_dir): PathBuf/,
  );
  assert.doesNotMatch(paths.split("#[cfg(test)]", 1)[0], /std::env/);
  for (const accessor of [
    "config_file",
    "instances_file",
    "instances_dir",
    "music_dir",
    "library_dir",
    "runtimes_dir",
    "accounts_file",
    "skins_dir",
    "operation_journal_file",
    "guardian_failure_memory_file",
    "known_good_dir",
    "persisted_state_rejection_streaks_file",
    "performance_dir",
    "performance_operations_dir",
    "benchmark_suites_dir",
    "benchmark_suite_drivers_dir",
    "launch_reports_dir",
    "user_mod_witness_file",
    "update_staging_dir",
  ]) {
    assert.match(paths, new RegExp(`pub fn ${accessor}\\(&self\\) -> &Path`));
  }
  assert.match(
    paths,
    /pub fn terminal_reset_scope\(&self\) -> TerminalResetScope/,
  );
  assert.match(paths, /pub fn contains_resolved\(&self, candidate: &Path\)/);
  assert.match(desktopCommands, /paths\.terminal_reset_scope\(\)/);
  assert.match(desktopCommands, /scope\s*\.contains_resolved\(/);
  assert.doesNotMatch(
    desktopCommands,
    /fn path_resolves_within|fn absolute_lexical/,
  );
  assert.doesNotMatch(
    runtimeLayout,
    /ManagedRuntimeCache::canonical|APPDATA|HOME/,
  );
  assert.match(
    state,
    /ManagedRuntimeCache::from_root\(\s*config\.paths\(\)\.runtimes_dir\(\)\.to_path_buf\(\)/,
  );
  assert.match(state, /config\.paths\(\) != init\.instances\.paths\(\)/);
  assert.match(state, /config\.paths\(\)\.performance_dir\(\)/);
  assert.match(
    state,
    /UpdaterStore::new\(config\.paths\(\)\.update_staging_dir\(\)\)/,
  );
  assert.doesNotMatch(instanceRegistry, /instances_dir\(\)\s*\.parent\(\)/);

  const exactConsumers = [
    [accounts, /paths\.accounts_file\(\)/],
    [skins, /paths\.skins_dir\(\)/],
    [failureMemory, /paths\.guardian_failure_memory_file\(\)/],
    [journals, /paths\.operation_journal_file\(\)/],
    [knownGood, /paths\.known_good_dir\(\)/],
    [rejectionStreaks, /paths\s*\.persisted_state_rejection_streaks_file\(\)/],
    [performanceOperations, /paths\.performance_operations_dir\(\)/],
    [benchmarkSuites, /paths\.benchmark_suites_dir\(\)/],
    [benchmarkSuiteDrivers, /paths\.benchmark_suite_drivers_dir\(\)/],
    [launchReports, /paths\.launch_reports_dir\(\)/],
    [userModWitness, /paths\.user_mod_witness_file\(\)/],
  ];
  for (const [source, expected] of exactConsumers)
    assert.match(source, expected);

  const purposeConsumers = exactConsumers
    .map(([source]) => source.split("#[cfg(test)]", 1)[0])
    .join("\n");
  assert.doesNotMatch(
    purposeConsumers,
    /\.join\("(?:accounts\.json|skins|state|guardian|performance|benchmarks|updates)"\)/,
  );
  assert.doesNotMatch(paths, /pub fn (?:state|benchmarks?)_dir\s*\(/);

  assert.match(updater, /pub fn new\(staging_dir: impl Into<PathBuf>\)/);
  assert.doesNotMatch(updater, /UPDATE_STAGING_DIR_NAME|\.join\("updates"\)/);
  assert.match(
    performanceManager,
    /pub fn load_for_startup\(performance_dir: &Path\)/,
  );
  assert.match(
    performanceRulesCache,
    /performance_dir\.join\(RULES_CACHE_FILE\)/,
  );
  assert.doesNotMatch(performanceRulesCache, /join\("performance"\)/);
  assert.match(
    performanceRulesState,
    /claim_rules_authority\(performance_dir\)/,
  );
  assert.match(skinCache, /skins_dir\s*\.join\(PROFILE_SKIN_FILE_CACHE_DIR\)/);
  assert.doesNotMatch(skinCache, /join\("skins"\)/);
  assert.doesNotMatch(flagsRoute, /remote-cache\.json|RETIRED_CACHE/);

  const appPathConsumers = [
    state,
    instanceRegistry,
    accounts,
    skins,
    failureMemory,
    journals,
    knownGood,
    rejectionStreaks,
    performanceOperations,
    benchmarkSuites,
    benchmarkSuiteDrivers,
    launchReports,
    userModWitness,
    apiMain,
    desktopMain,
    desktopCommands,
  ].join("\n");
  assert.doesNotMatch(appPathConsumers, /(?:\.paths\(\)|\bpaths)\s*\.root\(\)/);

  const apiResolution =
    "resolve_app_paths(app_root_selection_from_environment()?)?";
  assert.equal(apiMain.split(apiResolution).length - 1, 1);
  assert.ok(
    apiMain.indexOf(apiResolution) <
      apiMain.indexOf("ConfigStore::load_for_startup"),
  );

  const contextGeneration = "let mut context = tauri::generate_context!();";
  const desktopSelection = "desktop_app_root_selection_from_environment(";
  assert.equal(desktopMain.split(desktopSelection).length - 1, 1);
  assert.ok(
    desktopMain.indexOf(contextGeneration) <
      desktopMain.indexOf(desktopSelection),
  );
  assert.ok(
    desktopMain.indexOf(desktopSelection) <
      desktopMain.indexOf("ConfigStore::load_for_startup"),
  );
  assert.match(desktopMain, /context\.config\(\)\.identifier\.as_str\(\)/);
  assert.doesNotMatch(desktopMain, /app_root_selection_from_environment\(\)/);
  assert.doesNotMatch(desktopMain, /debug_assertions|current_dir/);

  for (const binary of [apiMain, desktopMain]) {
    assert.doesNotMatch(binary, /AppPaths::detect/);
    assert.match(binary, /paths\.performance_dir\(\)/);
  }

  assert.match(
    desktopBuild,
    /if env::var\("PROFILE"\)\.as_deref\(\) == Ok\("release"\) \{\s*return;/,
  );
  assert.match(desktopBuild, /fs::read_to_string\(DEV_TAURI_CONFIG\)/);
  assert.match(
    desktopBuild,
    /println!\("cargo:rustc-env=TAURI_CONFIG=\{config\}"\)/,
  );
  assert.match(
    desktopBuild,
    /fn main\(\) \{\s*apply_dev_tauri_config\(\);\s*tauri_build::build\(\)\s*\}/,
  );
  const mergeDevConfig = "merge_json(&mut config, dev_config);";
  const encodeMergedConfig =
    'let config = serde_json::to_string(&config).expect("failed to encode dev Tauri config");';
  const emitMergedConfig = 'println!("cargo:rustc-env=TAURI_CONFIG={config}");';
  for (const marker of [mergeDevConfig, encodeMergedConfig, emitMergedConfig]) {
    assert.notEqual(desktopBuild.indexOf(marker), -1);
  }
  assert.ok(
    desktopBuild.indexOf(mergeDevConfig) <
      desktopBuild.indexOf(encodeMergedConfig),
  );
  assert.ok(
    desktopBuild.indexOf(encodeMergedConfig) <
      desktopBuild.indexOf(emitMergedConfig),
  );

  assert.equal(JSON.parse(productionTauri).identifier, "dev.mateoltd.axial");
  assert.equal(
    JSON.parse(developmentTauri).identifier,
    "dev.mateoltd.axial.dev",
  );
  for (const task of ["dev", "dev:windows", "api"]) {
    assert.match(
      taskBody(taskfile, task),
      /^    env:\n(?:^      .*\n)*^      AXIAL_APP_ROOT_MODE: development$/m,
    );
  }
  for (const task of [
    "build",
    "build:windows",
    "build:api:release",
    "bundle",
  ]) {
    assert.doesNotMatch(taskBody(taskfile, task), /AXIAL_APP_ROOT_MODE/);
  }
  for (const task of ["build:dev", "build:windows:dev"]) {
    const body = taskBody(taskfile, task);
    assert.doesNotMatch(body, /AXIAL_APP_ROOT_MODE|--release/);
    assert.match(body, /cargo build --locked -p axial-desktop/);
  }
});
