//! Command construction from verified, retained feature capabilities.

use super::{
    jvm,
    model::{LaunchOptions, LaunchPlanError, LaunchPlanRequest, ValidatedLaunchCommand},
};
use crate::runtime::model::JavaSelectionRequirement;
use axial_minecraft::{
    LaunchVars, build_classpath, default_environment, effective_java_version_for,
    resolve_arguments, resolve_libraries,
};
use std::collections::BTreeMap;

pub(crate) fn build(request: LaunchPlanRequest) -> Result<ValidatedLaunchCommand, LaunchPlanError> {
    request
        .library_operation
        .validate_read_projection(&request.library_dir)
        .map_err(|_| LaunchPlanError::LibraryChanged)?;
    let version = request.installed.version();
    let java = effective_java_version_for(
        &request.target_version_id,
        &version.kind,
        &version.java_version,
    );
    JavaSelectionRequirement::for_current_host(java.major_version as u32)
        .validate(request.runtime.info(), request.runtime.architecture())
        .map_err(|_| LaunchPlanError::RuntimeIncompatible)?;
    if !valid_player_name(&request.auth)
        || request.auth.uuid.len() != 32
        || !request.auth.uuid.bytes().all(|b| b.is_ascii_hexdigit())
        || request.auth.access_token.is_empty()
    {
        return Err(LaunchPlanError::InvalidAccount);
    }
    let settings = &request.settings;
    let preset = validate_options(settings, &request.target_version_id, request.runtime.info())?;
    let maximum = settings.max_memory_mb.unwrap_or(4096);
    let minimum = settings.min_memory_mb.unwrap_or(512);
    let mut environment = default_environment();
    environment.features.insert(
        "has_custom_resolution".into(),
        settings.resolution.is_some(),
    );
    let libraries = resolve_libraries(version, &request.library_dir, &environment);
    if let Some(natives) = &request.prepared_natives {
        natives
            .validate_sources(&libraries)
            .map_err(|_| LaunchPlanError::NativesUnavailable)?;
    } else if libraries.iter().any(|library| library.is_native) {
        return Err(LaunchPlanError::NativesUnavailable);
    }
    let classpath = build_classpath(&libraries, Some(request.installed.client_jar()));
    let natives = request
        .prepared_natives
        .as_ref()
        .map(|prepared| prepared.path().to_string_lossy().into_owned())
        .unwrap_or_default();
    let (width, height) = settings
        .resolution
        .map(|(w, h)| (w.to_string(), h.to_string()))
        .unwrap_or_default();
    // The installation verifier authenticates this index and its virtual tree.
    let virtual_assets = request.installed.virtual_assets();
    let vars = LaunchVars {
        auth_player_name: request.auth.player_name.clone(),
        version_name: version.id.clone(),
        game_directory: request.game_dir.to_string_lossy().into_owned(),
        assets_root: request
            .library_dir
            .join("assets")
            .to_string_lossy()
            .into_owned(),
        asset_index_name: version.asset_index.id.clone(),
        auth_uuid: request.auth.uuid.clone(),
        auth_access_token: request.auth.access_token.clone(),
        client_id: request.auth.client_id.clone(),
        auth_xuid: request.auth.xuid.clone(),
        user_type: request.auth.user_type.clone(),
        version_type: version.kind.clone(),
        launcher_name: settings.launcher_name.clone(),
        launcher_version: settings.launcher_version.clone(),
        natives_directory: natives.clone(),
        classpath,
        library_directory: request
            .library_dir
            .join("libraries")
            .to_string_lossy()
            .into_owned(),
        classpath_separator: if cfg!(windows) { ";" } else { ":" }.into(),
        resolution_width: width,
        resolution_height: height,
        game_assets: if virtual_assets {
            request
                .library_dir
                .join("assets/virtual/legacy")
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        },
    };
    let (mut args, game_args) = resolve_arguments(version, &environment, &vars);
    args.extend(jvm::boot_throttle_args(
        request.runtime.info().major,
        settings.logical_cores,
    ));
    args.extend(jvm::gc_preset_args(
        &preset,
        request.runtime.info(),
        settings.low_impact_startup,
    ));
    args.extend([format!("-Xmx{maximum}M"), format!("-Xms{minimum}M")]);
    configure_native_paths(&mut args, &natives);
    if request.auth.is_offline()
        && matches!(request.target_version_id.as_str(), "1.16.4" | "1.16.5")
    {
        args.push("-Dminecraft.api.env=custom".into());
        for service in ["auth", "account", "session", "services"] {
            args.push(format!(
                "-Dminecraft.api.{service}.host=https://nope.invalid"
            ));
        }
    }
    if matches!(
        request.target_version_id.as_str(),
        "1.5" | "1.5.1" | "1.5.2"
    ) && version
        .libraries
        .iter()
        .any(|library| library.name == "net.minecraftforge:legacyfixer:1.0")
    {
        args.push("-Dfml.core.libraries.mirror=https://web.archive.org/web/20200830040255if_/http://files.minecraftforge.net/fmllibs/%s".into());
    }
    args.extend(settings.extra_jvm_args.iter().cloned());
    if version.main_class.is_empty()
        || version.main_class.starts_with('-')
        || !version
            .main_class
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '$'))
    {
        return Err(LaunchPlanError::InvalidCommand);
    }
    args.push(version.main_class.clone());
    args.extend(game_args);
    if args.len() > 16_384
        || args.iter().map(String::len).sum::<usize>() > 2 * 1024 * 1024
        || args.iter().any(|value| {
            value.contains("${") || value.chars().any(|c| c == '\0' || c == '\n' || c == '\r')
        })
    {
        return Err(LaunchPlanError::InvalidCommand);
    }
    let command = ValidatedLaunchCommand {
        program: request.runtime.executable().to_owned(),
        args,
        env: BTreeMap::new(),
        cwd: request.game_dir,
        library_operation: request.library_operation,
        library_dir: request.library_dir,
        version_guard: request.version_guard,
        installed: request.installed,
        prepared_natives: request.prepared_natives,
        game_libraries: None,
        runtime: request.runtime,
        managed_launch: request.managed_launch,
    };
    command.revalidate()?;
    Ok(command)
}

fn configure_native_paths(args: &mut Vec<String>, natives: &str) {
    // Verified native payloads are immutable. Runtime extraction and scratch
    // use ordinary JVM/library defaults, never this exact-cleanup directory.
    args.retain(|arg| !sets_runtime_temp_directory(arg));
    if !natives.is_empty() {
        args.push(format!("-Djava.library.path={natives}"));
    }
}

fn sets_runtime_temp_directory(arg: &str) -> bool {
    let Some(property) = arg.strip_prefix("-D") else {
        return false;
    };
    let key = property.split_once('=').map_or(property, |(key, _)| key);
    [
        "java.io.tmpdir",
        "jna.tmpdir",
        "org.lwjgl.system.SharedLibraryExtractPath",
        "io.netty.native.workdir",
    ]
    .contains(&key)
}

fn valid_player_name(auth: &super::model::LaunchAuthContext) -> bool {
    if auth.is_offline() {
        crate::settings::validate_username(&auth.player_name).is_ok()
    } else {
        // Microsoft accepts historical one- and two-character profile names.
        // Offline account creation retains the stricter local naming rule.
        !auth.player_name.is_empty()
            && auth.player_name.len() <= 16
            && auth
                .player_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }
}

pub(crate) fn validate_options(
    settings: &LaunchOptions,
    target: &str,
    info: &axial_minecraft::JavaRuntimeInfo,
) -> Result<String, LaunchPlanError> {
    if let Some((width, height)) = settings.resolution {
        if !(320..=16384).contains(&width) || !(320..=16384).contains(&height) {
            return Err(LaunchPlanError::InvalidResolution);
        }
    }
    let maximum = settings.max_memory_mb.unwrap_or(4096);
    let minimum = settings.min_memory_mb.unwrap_or(512);
    if !(512..=1024 * 1024).contains(&maximum) || !(256..=maximum).contains(&minimum) {
        return Err(LaunchPlanError::InvalidMemory);
    }
    validate_extra_args(&settings.extra_jvm_args, info)?;
    if settings.jvm_preset.is_empty() {
        Ok(jvm::auto_select_preset_with_host(
            target,
            &settings.loader,
            settings.is_modded,
            info,
            Some(settings.logical_cores),
            settings.total_memory_mb,
        ))
    } else {
        let selected = jvm::sanitize_preset(
            &settings.jvm_preset,
            target,
            &settings.loader,
            settings.is_modded,
            info,
        );
        if selected != settings.jvm_preset {
            return Err(LaunchPlanError::IncompatiblePreset);
        }
        Ok(selected)
    }
}

pub(crate) fn split_extra_args(input: &str) -> Result<Vec<String>, LaunchPlanError> {
    if input.len() > 8192 {
        return Err(LaunchPlanError::InvalidJvmArguments);
    }
    shlex::split(input).ok_or(LaunchPlanError::InvalidJvmArguments)
}

fn validate_extra_args(
    args: &[String],
    info: &axial_minecraft::JavaRuntimeInfo,
) -> Result<(), LaunchPlanError> {
    let unlock = args
        .iter()
        .position(|arg| arg == "-XX:+UnlockExperimentalVMOptions");
    for (index, arg) in args.iter().enumerate() {
        if arg.is_empty() || !arg.starts_with('-') || arg.chars().any(char::is_control) {
            return Err(LaunchPlanError::InvalidJvmArguments);
        }
        if arg.starts_with("-Xmx")
            || arg.starts_with("-Xms")
            || arg.starts_with("-javaagent")
            || arg.starts_with("-agent")
            || arg.starts_with("-Xbootclasspath")
            || sets_runtime_temp_directory(arg)
            || [
                "-cp",
                "-classpath",
                "--class-path",
                "-jar",
                "-m",
                "--module",
                "--module-path",
                "-p",
            ]
            .iter()
            .any(|reserved| arg == reserved || arg.starts_with(&format!("{reserved}=")))
            || arg.strip_prefix("-D").is_some_and(|property| {
                [
                    "java.class.path",
                    "java.library.path",
                    "java.home",
                    "org.lwjgl.librarypath",
                    "minecraft.applet.TargetDirectory",
                ]
                .contains(&property.split_once('=').map_or(property, |(key, _)| key))
            })
        {
            return Err(LaunchPlanError::ReservedJvmArgument);
        }
        if (arg == "-XX:+UseShenandoahGC" && !jvm::supports_shenandoah(info))
            || (arg == "-XX:+UseZGC" && !jvm::supports_zgc(info))
            || (arg == "-XX:+ZGenerational" && !jvm::supports_generational_zgc(info))
        {
            return Err(LaunchPlanError::UnsupportedJvmOption);
        }
        if arg.starts_with("-XX:G1NewSizePercent=") || arg.starts_with("-XX:G1MaxNewSizePercent=") {
            if !jvm::supports_hotspot_tuning(info) {
                return Err(LaunchPlanError::UnsupportedJvmOption);
            }
            if unlock.is_none_or(|unlock| unlock > index) {
                return Err(LaunchPlanError::JvmOptionOrdering);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_command_binds_applet_home_to_the_admitted_game_directory() {
        let (_root, prepared, _application) = super::super::prepare::tests::legacy_fixture().await;
        prepared.validate_before_spawn().unwrap();
        let command = prepared.validated_command();
        assert!(command.installed.version().is_legacy_version());
        assert_eq!(command.runtime.info().major, 8);
        let game = prepared
            .instance()
            .game_directory()
            .read_projection()
            .unwrap();
        let game = game.to_str().unwrap();
        assert!(game.contains(' '));
        assert_eq!(command.cwd().to_str().unwrap(), game);
        let main = command
            .args()
            .iter()
            .position(|arg| arg == &command.installed.version().main_class)
            .unwrap();
        let properties = command.args()[..main]
            .iter()
            .filter(|arg| {
                arg.as_str() == "-Dminecraft.applet.TargetDirectory"
                    || arg.starts_with("-Dminecraft.applet.TargetDirectory=")
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            properties,
            [format!("-Dminecraft.applet.TargetDirectory={game}")]
        );
        assert!(
            command.args()[main + 1..]
                .windows(2)
                .any(|pair| pair == ["--gameDir", game])
        );
    }

    #[test]
    fn vanilla_1_20_1_keeps_verified_native_search_paths_out_of_runtime_scratch() {
        let version: axial_minecraft::VersionJson = serde_json::from_str(include_str!(
            "../../../../acceptance/fixtures/providers/mojang-1.20.1.json"
        ))
        .expect("real Mojang 1.20.1 manifest");
        let vars = LaunchVars {
            auth_player_name: "Player".into(),
            version_name: version.id.clone(),
            game_directory: "/instance".into(),
            assets_root: "/library/assets".into(),
            asset_index_name: version.asset_index.id.clone(),
            auth_uuid: axial_minecraft::offline_uuid("Player"),
            auth_access_token: "0".into(),
            client_id: String::new(),
            auth_xuid: String::new(),
            user_type: "msa".into(),
            version_type: "release".into(),
            launcher_name: "Axial".into(),
            launcher_version: "test".into(),
            natives_directory: "/verified/natives".into(),
            classpath: "verified-client.jar".into(),
            library_directory: "/library/libraries".into(),
            classpath_separator: ":".into(),
            resolution_width: String::new(),
            resolution_height: String::new(),
            game_assets: String::new(),
        };
        let (mut args, game) = resolve_arguments(&version, &default_environment(), &vars);
        assert!(args.contains(&"-Djava.library.path=/verified/natives".into()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("-Dorg.lwjgl.librarypath="))
        );
        assert_eq!(
            args.iter()
                .filter(|arg| sets_runtime_temp_directory(arg))
                .count(),
            3
        );
        configure_native_paths(&mut args, &vars.natives_directory);
        assert!(!args.iter().any(|arg| sets_runtime_temp_directory(arg)));
        assert!(args.contains(&"-Djava.library.path=/verified/natives".into()));
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("-Dorg.lwjgl.librarypath=")),
            "the launcher must preserve standard JNI lookup for legacy native filenames"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-cp", "verified-client.jar"])
        );
        assert!(game.windows(2).any(|pair| pair == ["--username", "Player"]));
        assert!(args.iter().chain(&game).all(|arg| !arg.contains("${")));
    }

    #[test]
    fn runtime_temp_directories_cannot_be_reintroduced_by_provider_or_custom_arguments() {
        for key in [
            "java.io.tmpdir",
            "jna.tmpdir",
            "org.lwjgl.system.SharedLibraryExtractPath",
            "io.netty.native.workdir",
        ] {
            for argument in [format!("-D{key}=/verified/natives"), format!("-D{key}")] {
                assert_eq!(
                    validate_extra_args(&[argument.clone()], &info()),
                    Err(LaunchPlanError::ReservedJvmArgument)
                );
                let mut provider_args = vec![argument, "-Dunrelated.tmpdir=value".into()];
                configure_native_paths(&mut provider_args, "");
                assert_eq!(provider_args, ["-Dunrelated.tmpdir=value"]);
            }
        }
        assert!(validate_extra_args(&["-Djna.tmpdir.other=value".into()], &info()).is_ok());
    }

    #[test]
    fn authenticated_historical_names_do_not_use_offline_creation_limits() {
        use super::super::model::LaunchAuthContext;
        for name in ["a", "ab", "Valid_Name"] {
            let mut online = LaunchAuthContext::offline(name);
            online.access_token = "authenticated-provider-token".into();
            assert!(valid_player_name(&online), "{name}");
        }
        for name in ["", "a b", "12345678901234567", "a\n"] {
            let mut online = LaunchAuthContext::offline(name);
            online.access_token = "authenticated-provider-token".into();
            assert!(!valid_player_name(&online), "{name:?}");
        }
        assert!(!valid_player_name(&LaunchAuthContext::offline("ab")));
    }
    fn info() -> axial_minecraft::JavaRuntimeInfo {
        axial_minecraft::JavaRuntimeInfo {
            id: String::new(),
            major: 17,
            update: 10,
            distribution: "temurin".into(),
            path: String::new(),
        }
    }
    #[test]
    fn explicit_arguments_cannot_replace_owned_entrypoints_or_paths() {
        for arg in [
            "-cp",
            "--class-path=elsewhere",
            "-javaagent:agent.jar",
            "-Xmx8G",
            "-Djava.library.path=elsewhere",
            "-Dorg.lwjgl.librarypath=elsewhere",
            "-Dminecraft.applet.TargetDirectory=elsewhere",
            "-Djava.class.path",
            "-Djava.library.path",
            "-Djava.home",
            "-Dorg.lwjgl.librarypath",
            "-Dminecraft.applet.TargetDirectory",
            "@args",
            "Main",
        ] {
            assert!(
                validate_extra_args(&[arg.into()], &info()).is_err(),
                "{arg}"
            );
        }
        assert!(validate_extra_args(&["-Dexample=value".into()], &info()).is_ok());
    }
    #[test]
    fn quoting_and_experimental_option_order_are_preserved() {
        assert_eq!(
            split_extra_args("-Dlabel='with spaces'").unwrap(),
            ["-Dlabel=with spaces"]
        );
        assert_eq!(
            split_extra_args("-Dlabel='unfinished"),
            Err(LaunchPlanError::InvalidJvmArguments)
        );
        assert_eq!(
            validate_extra_args(
                &[
                    "-XX:G1NewSizePercent=20".into(),
                    "-XX:+UnlockExperimentalVMOptions".into()
                ],
                &info()
            ),
            Err(LaunchPlanError::JvmOptionOrdering)
        );
    }
}
