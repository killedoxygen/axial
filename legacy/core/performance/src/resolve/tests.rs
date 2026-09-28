use super::app_version::validate_app_version_compatibility_with_running;
use super::hardware::{
    decode_windows_command_output, gpu_vendor_from_model_name, gpu_vendor_from_pci_id,
    is_drm_card_path, nvidia_arch_from_model, nvidia_model_from_information,
    parse_windows_gpu_names, select_gpu_from_names, select_gpu_vendor_from_vendors,
};
use super::{
    PERFORMANCE_MANIFEST_SCHEMA_VERSION, ResolveError, builtin_manifest, parse_mode, resolve_plan,
    validate_manifest,
};
use crate::types::{
    CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget, HardwareProfile,
    HardwareRequirement, ManagedMod, Manifest, ModCondition, OwnershipClass, PerformanceMode,
    ResolutionRequest, VersionFamily,
};
use std::path::Path;

const FAMILY_F_FABRIC_CORE_ADDITIONS: &[&str] = &[
    "scalablelux",
    "particle-core",
    "threadtweak",
    "badoptimizations",
];
const MODERNFIX_EXACT_GAME_VERSIONS: &[&str] = &[
    "1.16.4", "1.16.5", "1.18.2", "1.19.2", "1.19.4", "1.20", "1.20.1", "1.20.2", "1.20.3",
    "1.20.4", "1.20.5", "1.20.6", "1.21", "1.21.1", "1.21.4", "26.1", "26.1.2",
];

#[test]
fn families_a_b_d_managed_plans_resolve_named_vanilla_enhanced_compositions() {
    let manifest = builtin_manifest().expect("manifest");

    for (game_version, family, composition_id) in [
        ("1.5.2", VersionFamily::A, "family-a-vanilla-enhanced"),
        ("1.7.10", VersionFamily::B, "family-b-vanilla-enhanced"),
        ("1.15.2", VersionFamily::D, "family-d-vanilla-enhanced"),
    ] {
        for loader in ["vanilla", "fabric", "forge", "neoforge", "quilt"] {
            let plan = resolve_plan(
                Some(&manifest),
                ResolutionRequest {
                    game_version: game_version.to_string(),
                    loader: loader.to_string(),
                    mode: PerformanceMode::Managed,
                    hardware: HardwareProfile::default(),
                    installed_mods: Vec::new(),
                },
            );

            assert_eq!(plan.composition_id, composition_id);
            assert_eq!(plan.family, family);
            assert_eq!(plan.loader, loader);
            assert_eq!(plan.tier, CompositionTier::VanillaEnhanced);
            assert!(plan.mods.is_empty());
            assert!(plan.fallback_reason.is_empty());
        }
    }
}

#[test]
fn family_c_forge_1_12_2_resolves_conservative_core_with_vanilla_fallback() {
    let manifest = builtin_manifest().expect("manifest");

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.12.2".to_string(),
            loader: "forge".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-c-forge-core");
    assert_eq!(plan.family, VersionFamily::C);
    assert_eq!(plan.loader, "forge");
    assert_eq!(plan.tier, CompositionTier::Core);
    assert_eq!(count_mods_with_slug(&plan.mods, "foamfix"), 1);
    assert_eq!(count_mods_with_slug(&plan.mods, "ai-improvements"), 1);
    assert_eq!(count_mods_with_slug(&plan.mods, "clumps"), 1);
    assert!(plan.fallback_reason.is_empty());
}

#[test]
fn family_c_non_forge_loaders_stay_on_vanilla_enhanced() {
    let manifest = builtin_manifest().expect("manifest");

    for loader in ["vanilla", "fabric", "neoforge", "quilt"] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.12.2".to_string(),
                loader: loader.to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.composition_id, "family-c-vanilla-enhanced");
        assert_eq!(plan.family, VersionFamily::C);
        assert_eq!(plan.loader, loader);
        assert_eq!(plan.tier, CompositionTier::VanillaEnhanced);
        assert!(plan.mods.is_empty());
        assert!(plan.fallback_reason.is_empty());
    }
}

#[test]
fn family_c_forge_core_emergency_disable_falls_back_to_vanilla_enhanced() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.emergency_disables.push(test_composition_disable(
        "hold-family-c-forge-core",
        "family-c-forge-core",
    ));

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.12.2".to_string(),
            loader: "forge".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-c-vanilla-enhanced");
    assert_eq!(plan.family, VersionFamily::C);
    assert_eq!(plan.loader, "forge");
    assert_eq!(plan.tier, CompositionTier::VanillaEnhanced);
    assert!(plan.mods.is_empty());
    assert_eq!(
        plan.fallback_reason,
        "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option."
    );
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("family-c-forge-core skipped by emergency disable")
            && warning.contains("Temporary hold.")
    }));
}

#[test]
fn fabric_family_e_and_f_managed_plans_resolve_real_mods() {
    let manifest = builtin_manifest().expect("manifest");

    for (game_version, expected_family, expects_threadtweak) in [
        ("1.20.1", VersionFamily::E, false),
        ("1.20.4", VersionFamily::F, true),
    ] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(plan.family, expected_family);
        assert_eq!(plan.loader, "fabric");
        assert!(plan.composition_id.contains("fabric"));
        assert!(
            plan.mods
                .iter()
                .any(|managed_mod| managed_mod.slug == "sodium")
        );
        assert_eq!(
            count_mods_with_slug(&plan.mods, "threadtweak"),
            usize::from(expects_threadtweak)
        );
    }
}

#[test]
fn managed_fabric_plans_bound_ebe_and_memory_leak_fix_to_supported_versions() {
    let manifest = builtin_manifest().expect("manifest");

    for (game_version, expects_ebe, expects_memory_leak_fix) in [
        ("1.16.1", false, true),
        ("1.16.2", true, true),
        ("1.20.4", true, true),
        ("1.21.4", true, false),
        ("1.21.5", false, false),
        ("1.21.11", false, false),
    ] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(
            count_mods_with_slug(&plan.mods, "ebe"),
            usize::from(expects_ebe),
            "{game_version} EBE"
        );
        assert_eq!(
            count_mods_with_slug(&plan.mods, "memoryleakfix"),
            usize::from(expects_memory_leak_fix),
            "{game_version} MemoryLeakFix"
        );
    }
}

#[test]
fn current_family_f_plans_omit_unsupported_ebe_and_memory_leak_fix() {
    let manifest = builtin_manifest().expect("manifest");

    for loader in ["fabric", "forge", "neoforge"] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: "1.21.11".to_string(),
                loader: loader.to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(count_mods_with_slug(&plan.mods, "ebe"), 0, "{loader}");
        assert_eq!(
            count_mods_with_slug(&plan.mods, "memoryleakfix"),
            0,
            "{loader}"
        );
    }
}

#[test]
fn modernfix_uses_exact_supported_roots_once_without_parent_fallback() {
    let manifest = builtin_manifest().expect("manifest");

    for game_version in MODERNFIX_EXACT_GAME_VERSIONS {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: (*game_version).to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(
            count_mods_with_slug(&plan.mods, "modernfix"),
            1,
            "{game_version}"
        );
    }

    for game_version in ["1.21.11", "26.2"] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            },
        );

        assert_eq!(count_mods_with_slug(&plan.mods, "modernfix"), 0);
    }
}

#[test]
fn builtin_manifest_binds_all_managed_artifacts_to_immutable_provider_identities() {
    let manifest = builtin_manifest().expect("manifest");
    let expected_project_ids = [
        ("sodium", "AANobbMI"),
        ("lithium", "gvQqBUqZ"),
        ("ferrite-core", "uXXizFIs"),
        ("immediatelyfast", "5ZwdcRci"),
        ("dynamic-fps", "LQ3K71Q1"),
        ("modernfix", "nmDcB62a"),
        ("entityculling", "NNAgCjsB"),
        ("c2me-fabric", "VSNURh3q"),
        ("moreculling", "51shyZVL"),
        ("krypton", "fQEb0iXm"),
        ("enhancedblockentities", "OVuFYfre"),
        ("memoryleakfix", "NRjRiSSD"),
        ("clumps", "Wnxd13zP"),
        ("foamfix", "jupr7Bf5"),
        ("ai-improvements", "DSVgwcji"),
        ("lazydfu", "hvFnDODi"),
        ("smooth-boot-reloaded", "z53V2L4P"),
        ("starlight", "H8CaAYZC"),
        ("nvidium", "SfMw2IZN"),
        ("embeddium", "sk9rgfiA"),
        ("scalablelux", "Ps1zyz6x"),
        ("particle-core", "RSeLon5O"),
        ("threadtweak", "vSEH1ERy"),
        ("badoptimizations", "g96Z4WVZ"),
    ];
    assert_eq!(manifest.artifacts.len(), expected_project_ids.len());
    for (artifact_id, project_id) in expected_project_ids {
        let artifact = manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.id == artifact_id)
            .expect("audited artifact authority");
        assert_eq!(artifact.source.project_id, project_id, "{artifact_id}");
        assert!(
            manifest
                .compositions
                .iter()
                .flat_map(|composition| &composition.mods)
                .filter(|managed_mod| managed_mod.artifact_id == artifact_id)
                .all(|managed_mod| managed_mod.project_id == project_id),
            "{artifact_id}"
        );
    }
    let ebe_artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.id == "enhancedblockentities")
        .expect("EBE artifact authority");

    assert_eq!(ebe_artifact.source.project_id, "OVuFYfre");
    assert_eq!(ebe_artifact.source.slug, "ebe");
    let ebe_declarations = manifest
        .compositions
        .iter()
        .flat_map(|composition| composition.mods.iter())
        .filter(|managed_mod| managed_mod.artifact_id == "enhancedblockentities")
        .collect::<Vec<_>>();
    assert_eq!(ebe_declarations.len(), 2);
    assert!(ebe_declarations.iter().all(|managed_mod| {
        managed_mod.project_id == "OVuFYfre"
            && managed_mod.slug == "ebe"
            && managed_mod.version_range == ">=1.16.2 <=1.21.4"
    }));

    let memory_leak_fix_artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.id == "memoryleakfix")
        .expect("MemoryLeakFix artifact authority");
    assert_eq!(memory_leak_fix_artifact.source.project_id, "NRjRiSSD");
    assert_eq!(memory_leak_fix_artifact.source.slug, "memoryleakfix");
    let memory_leak_fix_declarations = manifest
        .compositions
        .iter()
        .flat_map(|composition| composition.mods.iter())
        .filter(|managed_mod| managed_mod.artifact_id == "memoryleakfix")
        .collect::<Vec<_>>();
    assert_eq!(memory_leak_fix_declarations.len(), 4);
    assert!(memory_leak_fix_declarations.iter().all(|managed_mod| {
        managed_mod.project_id == "NRjRiSSD"
            && managed_mod.slug == "memoryleakfix"
            && managed_mod.version_range == "<=1.20.4"
    }));

    let modernfix_artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.id == "modernfix")
        .expect("ModernFix artifact authority");
    assert_eq!(modernfix_artifact.source.project_id, "nmDcB62a");
    assert_eq!(modernfix_artifact.source.slug, "modernfix");
    let modernfix_declarations = manifest
        .compositions
        .iter()
        .flat_map(|composition| composition.mods.iter())
        .filter(|managed_mod| managed_mod.artifact_id == "modernfix")
        .collect::<Vec<_>>();
    assert_eq!(modernfix_declarations.len(), 4);
    let expected_versions = MODERNFIX_EXACT_GAME_VERSIONS
        .iter()
        .map(|version| (*version).to_string())
        .collect::<Vec<_>>();
    assert!(modernfix_declarations.iter().all(|managed_mod| {
        managed_mod.project_id == "nmDcB62a"
            && managed_mod.slug == "modernfix"
            && managed_mod.version_range.is_empty()
            && managed_mod.exact_game_versions == expected_versions
    }));
    validate_manifest(&manifest).expect("corrected provider identities should validate");

    let mut stale_ebe = manifest.clone();
    let declaration = stale_ebe
        .compositions
        .iter_mut()
        .flat_map(|composition| composition.mods.iter_mut())
        .find(|managed_mod| managed_mod.artifact_id == "enhancedblockentities")
        .expect("EBE composition declaration");
    declaration.project_id = "AANobbMI".to_string();
    assert_error_kind(
        validate_manifest(&stale_ebe),
        ResolveError::ManagedModProjectMismatch {
            artifact_id: "enhancedblockentities".to_string(),
            expected: "OVuFYfre".to_string(),
            actual: "AANobbMI".to_string(),
        },
    );

    let mut stale_memory_leak_fix = manifest;
    let declaration = stale_memory_leak_fix
        .compositions
        .iter_mut()
        .flat_map(|composition| composition.mods.iter_mut())
        .find(|managed_mod| managed_mod.artifact_id == "memoryleakfix")
        .expect("MemoryLeakFix composition declaration");
    declaration.project_id = "AANobbMI".to_string();
    assert_error_kind(
        validate_manifest(&stale_memory_leak_fix),
        ResolveError::ManagedModProjectMismatch {
            artifact_id: "memoryleakfix".to_string(),
            expected: "NRjRiSSD".to_string(),
            actual: "AANobbMI".to_string(),
        },
    );
}

#[test]
fn forge_family_e_and_f_resolve_extended_then_core_when_extended_disabled() {
    for (
        game_version,
        expected_family,
        extended_composition_id,
        core_composition_id,
        _vanilla_composition_id,
    ) in [
        (
            "1.20.1",
            VersionFamily::E,
            "family-e-forge-extended",
            "family-e-forge-core",
            "family-e-vanilla-enhanced",
        ),
        (
            "1.20.4",
            VersionFamily::F,
            "family-f-forge-extended",
            "family-f-forge-core",
            "family-f-vanilla-enhanced",
        ),
    ] {
        for loader in ["forge", "neoforge"] {
            let manifest = builtin_manifest().expect("manifest");
            let plan = resolve_plan(
                Some(&manifest),
                ResolutionRequest {
                    game_version: game_version.to_string(),
                    loader: loader.to_string(),
                    mode: PerformanceMode::Managed,
                    hardware: HardwareProfile::default(),
                    installed_mods: Vec::new(),
                },
            );

            assert_eq!(plan.composition_id, extended_composition_id);
            assert_eq!(plan.family, expected_family);
            assert_eq!(plan.loader, loader);
            assert_eq!(plan.tier, CompositionTier::Extended);
            assert_eq!(count_mods_with_slug(&plan.mods, "embeddium"), 1);
            assert_eq!(count_mods_with_slug(&plan.mods, "ferrite-core"), 1);

            let mut disabled_manifest = builtin_manifest().expect("manifest");
            disabled_manifest
                .emergency_disables
                .push(test_composition_disable(
                    "hold-forge-extended",
                    extended_composition_id,
                ));

            let fallback_plan = resolve_plan(
                Some(&disabled_manifest),
                ResolutionRequest {
                    game_version: game_version.to_string(),
                    loader: loader.to_string(),
                    mode: PerformanceMode::Managed,
                    hardware: HardwareProfile::default(),
                    installed_mods: Vec::new(),
                },
            );

            assert_eq!(fallback_plan.composition_id, core_composition_id);
            assert_eq!(fallback_plan.family, expected_family);
            assert_eq!(fallback_plan.loader, loader);
            assert_eq!(fallback_plan.tier, CompositionTier::Core);
            assert_eq!(count_mods_with_slug(&fallback_plan.mods, "embeddium"), 1);
            assert_eq!(count_mods_with_slug(&fallback_plan.mods, "ferrite-core"), 1);
            assert_eq!(
                fallback_plan.fallback_reason,
                "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option."
            );
            let expected_warning =
                format!("{extended_composition_id} skipped by emergency disable");
            assert!(fallback_plan.warnings.iter().any(|warning| {
                warning.contains(&expected_warning) && warning.contains("Temporary hold.")
            }));
        }
    }
}

#[test]
fn family_e_fabric_fallback_does_not_include_family_f_core_additions() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.emergency_disables.push(test_composition_disable(
        "hold-family-e-extended",
        "family-e-fabric-extended",
    ));

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.20.1".to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-e-fabric-core");
    assert_eq!(plan.family, VersionFamily::E);
    assert_eq!(plan.tier, CompositionTier::Core);
    for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
        assert_eq!(count_mods_with_slug(&plan.mods, slug), 0, "{slug}");
    }
}

#[test]
fn parse_mode_accepts_supported_values() {
    assert_eq!(parse_mode("managed"), Some(PerformanceMode::Managed));
    assert_eq!(parse_mode("vanilla"), Some(PerformanceMode::Vanilla));
    assert_eq!(parse_mode("custom"), Some(PerformanceMode::Custom));
    assert_eq!(parse_mode("invalid"), None);
}

#[test]
fn pci_vendor_ids_map_to_gpu_vendors() {
    assert_eq!(gpu_vendor_from_pci_id("0x10de\n"), Some("nvidia"));
    assert_eq!(gpu_vendor_from_pci_id("0X1002"), Some("amd"));
    assert_eq!(gpu_vendor_from_pci_id("0x1022"), Some("amd"));
    assert_eq!(gpu_vendor_from_pci_id(" 0x8086 "), Some("intel"));
    assert_eq!(gpu_vendor_from_pci_id("0x1234"), None);
}

#[test]
fn linux_drm_card_paths_accept_only_plain_card_nodes() {
    for path in ["/sys/class/drm/card0", "/sys/class/drm/card12"] {
        assert!(is_drm_card_path(Path::new(path)), "{path}");
    }

    for path in [
        "/sys/class/drm/card",
        "/sys/class/drm/card0-DP-1",
        "/sys/class/drm/renderD128",
        "/sys/class/drm/controlD64",
        "/sys/class/drm/cardx",
    ] {
        assert!(!is_drm_card_path(Path::new(path)), "{path}");
    }
}

#[test]
fn gpu_vendor_selection_prefers_nvidia_then_amd_then_intel() {
    assert_eq!(select_gpu_vendor_from_vendors(["intel"]), "intel");
    assert_eq!(select_gpu_vendor_from_vendors(["intel", "amd"]), "amd");
    assert_eq!(
        select_gpu_vendor_from_vendors(["intel", "amd", "nvidia"]),
        "nvidia"
    );
    assert_eq!(select_gpu_vendor_from_vendors(["unknown"]), "");
    assert_eq!(select_gpu_vendor_from_vendors([]), "");
}

#[test]
fn nvidia_model_strings_infer_arch_generation() {
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce GTX 1080"), 1);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce GTX 1070 Ti"), 1);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce GTX 1660 Ti"), 2);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 2060"), 2);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 2070 SUPER"), 2);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 3080"), 3);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 4090"), 4);
    assert_eq!(nvidia_arch_from_model("NVIDIA GeForce RTX 5090"), 4);
    assert_eq!(nvidia_arch_from_model("NVIDIA Quadro RTX 5000"), 2);
    assert_eq!(nvidia_arch_from_model("NVIDIA RTX A5000"), 3);
    assert_eq!(nvidia_arch_from_model("NVIDIA Quadro P2000"), 0);
    assert_eq!(nvidia_arch_from_model("Unknown GPU"), 0);
}

#[test]
fn nvidia_proc_information_parser_reads_model_line_with_spacing_and_case() {
    assert_eq!(
        nvidia_model_from_information(
            "Model: \t\t NVIDIA GeForce RTX 3080\nIRQ: 54\nGPU UUID: GPU-test\n"
        )
        .as_deref(),
        Some("NVIDIA GeForce RTX 3080")
    );
    assert_eq!(
        nvidia_model_from_information("irq: 54\n model : nvidia geforce rtx 4070 \n").as_deref(),
        Some("nvidia geforce rtx 4070")
    );
    assert_eq!(
        nvidia_model_from_information("MoDeL:\tNVIDIA GeForce GTX 1660 SUPER\n").as_deref(),
        Some("NVIDIA GeForce GTX 1660 SUPER")
    );
    assert_eq!(nvidia_model_from_information("Model:\t \n"), None);
    assert_eq!(nvidia_model_from_information("IRQ: 54\n"), None);
}

#[test]
fn windows_gpu_output_parser_ignores_header_and_blank_lines() {
    assert_eq!(
        parse_windows_gpu_names("Name\r\n\r\n NVIDIA GeForce RTX 4070 \r\nIntel UHD\r\n"),
        vec![
            "NVIDIA GeForce RTX 4070".to_string(),
            "Intel UHD".to_string()
        ]
    );
    assert_eq!(
        parse_windows_gpu_names("name\nAMD Radeon RX 7900 XT\n"),
        vec!["AMD Radeon RX 7900 XT".to_string()]
    );
}

#[test]
fn windows_gpu_name_selection_is_platform_neutral() {
    assert_eq!(
        gpu_vendor_from_model_name("Advanced Micro Devices, Inc. Radeon RX 7800 XT"),
        Some("amd")
    );
    assert_eq!(
        gpu_vendor_from_model_name("Intel(R) UHD Graphics"),
        Some("intel")
    );
    assert_eq!(
        gpu_vendor_from_model_name("GeForce GTX 1660 SUPER"),
        Some("nvidia")
    );
    assert_eq!(gpu_vendor_from_model_name("Generic Display Adapter"), None);

    assert_eq!(
        select_gpu_from_names(&[
            "Intel(R) UHD Graphics".to_string(),
            "AMD Radeon RX 7900 XT".to_string(),
        ]),
        ("amd".to_string(), 0)
    );
    assert_eq!(
        select_gpu_from_names(&[
            "Intel(R) UHD Graphics".to_string(),
            "NVIDIA GeForce RTX 4070".to_string(),
            "AMD Radeon RX 7900 XT".to_string(),
        ]),
        ("nvidia".to_string(), 4)
    );
    assert_eq!(
        select_gpu_from_names(&["Generic Display Adapter".to_string()]),
        (String::new(), 0)
    );
}

#[test]
fn windows_command_output_decodes_utf8_and_utf16le() {
    assert_eq!(
        decode_windows_command_output(b"Name\r\nNVIDIA GeForce RTX 4070\r\n").as_deref(),
        Some("Name\r\nNVIDIA GeForce RTX 4070\r\n")
    );

    let with_bom = "\u{feff}Name\r\nNVIDIA GeForce RTX 4070\r\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    assert_eq!(
        decode_windows_command_output(&with_bom).as_deref(),
        Some("Name\r\nNVIDIA GeForce RTX 4070\r\n")
    );

    let without_bom = "Name\r\nIntel UHD Graphics\r\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    assert_eq!(
        decode_windows_command_output(&without_bom).as_deref(),
        Some("Name\r\nIntel UHD Graphics\r\n")
    );
    assert_eq!(decode_windows_command_output(b""), None);
}

#[test]
fn family_e_fabric_1_16_5_uses_older_version_gated_mods_without_nvidium() {
    let plan = fabric_plan("1.16.5", nvidia_turing_hardware());

    assert_eq!(plan.composition_id, "family-e-fabric-extended");
    assert_eq!(plan.family, VersionFamily::E);
    assert_eq!(plan.tier, CompositionTier::Extended);
    for slug in ["lazydfu", "smooth-boot-reloaded", "starlight"] {
        assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
    }
    assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 0);
    assert!(
        !plan
            .warnings
            .iter()
            .any(|warning| warning == "nvidium skipped: no NVIDIA Turing+ GPU detected")
    );
}

#[test]
fn family_e_fabric_1_20_1_uses_nvidium_without_older_version_gated_mods() {
    let plan = fabric_plan("1.20.1", nvidia_turing_hardware());

    assert_eq!(plan.composition_id, "family-e-fabric-extended");
    assert_eq!(plan.family, VersionFamily::E);
    assert_eq!(plan.tier, CompositionTier::Extended);
    assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 1);
    for slug in ["lazydfu", "smooth-boot-reloaded", "starlight"] {
        assert_eq!(count_mods_with_slug(&plan.mods, slug), 0, "{slug}");
    }
}

#[test]
fn family_f_fabric_1_20_4_uses_nvidium_and_family_f_additions() {
    let plan = fabric_plan("1.20.4", nvidia_turing_hardware());

    assert_eq!(plan.composition_id, "family-f-fabric-extended");
    assert_eq!(plan.family, VersionFamily::F);
    assert_eq!(plan.tier, CompositionTier::Extended);
    assert_eq!(count_mods_with_slug(&plan.mods, "nvidium"), 1);
    for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
        assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
    }
}

#[test]
fn nvidium_requires_nvidia_turing_or_newer_for_applicable_versions() {
    for game_version in ["1.20.1", "1.20.4"] {
        for (hardware, expected_included) in [
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 2,
                    ..HardwareProfile::default()
                },
                true,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 3,
                    ..HardwareProfile::default()
                },
                true,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 1,
                    ..HardwareProfile::default()
                },
                false,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "nvidia".to_string(),
                    gpu_arch: 0,
                    ..HardwareProfile::default()
                },
                false,
            ),
            (
                HardwareProfile {
                    gpu_vendor: "amd".to_string(),
                    gpu_arch: 0,
                    ..HardwareProfile::default()
                },
                false,
            ),
        ] {
            let plan = fabric_plan(game_version, hardware);

            assert_eq!(
                plan.mods
                    .iter()
                    .any(|managed_mod| managed_mod.slug == "nvidium"),
                expected_included,
                "{game_version}"
            );
            assert_eq!(
                plan.warnings
                    .iter()
                    .any(|warning| warning == "nvidium skipped: no NVIDIA Turing+ GPU detected"),
                !expected_included,
                "{game_version}"
            );
        }
    }
}

#[test]
fn nvidium_is_skipped_when_iris_is_installed() {
    let manifest = builtin_manifest().expect("manifest");
    let hardware = HardwareProfile {
        gpu_vendor: "nvidia".to_string(),
        gpu_arch: 2,
        ..HardwareProfile::default()
    };

    for game_version in ["1.20.1", "1.20.4"] {
        let plan = resolve_plan(
            Some(&manifest),
            ResolutionRequest {
                game_version: game_version.to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: hardware.clone(),
                installed_mods: vec!["iris".to_string()],
            },
        );

        assert_eq!(plan.tier, CompositionTier::Extended);
        assert!(
            plan.mods
                .iter()
                .all(|managed_mod| managed_mod.slug != "nvidium")
        );
        assert!(
            plan.warnings.iter().any(|warning| {
                warning == "nvidium skipped: incompatible with managed mod iris"
            })
        );
    }
}

#[test]
fn manifest_without_emergency_disables_is_not_current_schema() {
    let error = serde_json::from_value::<Manifest>(serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "minimum_app_version": "0.4.0-alpha",
        "rule_channel": "bundled",
        "artifacts": [],
        "compositions": []
    }))
    .expect_err("missing emergency_disables should be invalid current schema");

    assert!(error.to_string().contains("emergency_disables"));
}

#[test]
fn manifest_without_artifacts_is_not_current_schema() {
    let error = serde_json::from_value::<Manifest>(serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "minimum_app_version": "0.4.0-alpha",
        "rule_channel": "bundled",
        "compositions": [],
        "emergency_disables": []
    }))
    .expect_err("missing artifacts should be invalid current schema");

    assert!(error.to_string().contains("artifacts"));
}

#[test]
fn manifest_without_minimum_app_version_is_not_current_schema() {
    let error = serde_json::from_value::<Manifest>(serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "rule_channel": "bundled",
        "artifacts": [],
        "compositions": [],
        "emergency_disables": []
    }))
    .expect_err("missing minimum_app_version should be invalid current schema");

    assert!(error.to_string().contains("minimum_app_version"));
}

#[test]
fn manifest_without_rule_channel_is_not_current_schema() {
    let error = serde_json::from_value::<Manifest>(serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "minimum_app_version": "0.4.0-alpha",
        "artifacts": [],
        "compositions": [],
        "emergency_disables": []
    }))
    .expect_err("missing rule_channel should be invalid current schema");

    assert!(error.to_string().contains("rule_channel"));
}

#[test]
fn validation_rejects_incompatible_or_invalid_manifest_metadata() {
    let mut too_new = builtin_manifest().expect("manifest");
    too_new.minimum_app_version = "0.4.0".to_string();
    assert_error_kind(
        validate_manifest(&too_new),
        ResolveError::UnsupportedAppVersion {
            required: "0.4.0".to_string(),
            running: env!("CARGO_PKG_VERSION").to_string(),
        },
    );

    let mut invalid_version = builtin_manifest().expect("manifest");
    invalid_version.minimum_app_version = "latest".to_string();
    assert_error_kind(
        validate_manifest(&invalid_version),
        ResolveError::InvalidMinimumAppVersion("latest".to_string()),
    );

    let mut missing_version = builtin_manifest().expect("manifest");
    missing_version.minimum_app_version = String::new();
    assert_error_kind(
        validate_manifest(&missing_version),
        ResolveError::MissingMinimumAppVersion,
    );

    let mut unknown_channel = builtin_manifest().expect("manifest");
    unknown_channel.rule_channel = "nightly".to_string();
    assert_error_kind(
        validate_manifest(&unknown_channel),
        ResolveError::UnsupportedRuleChannel("nightly".to_string()),
    );

    let mut missing_channel = builtin_manifest().expect("manifest");
    missing_channel.rule_channel = String::new();
    assert_error_kind(
        validate_manifest(&missing_channel),
        ResolveError::MissingRuleChannel,
    );
}

#[test]
fn validation_rejects_schema_v1_without_compatibility() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.schema_version = PERFORMANCE_MANIFEST_SCHEMA_VERSION - 1;

    assert_error_kind(
        validate_manifest(&manifest),
        ResolveError::UnsupportedSchema,
    );
}

#[test]
fn validation_accepts_current_and_older_minimum_app_versions() {
    for minimum_app_version in [env!("CARGO_PKG_VERSION"), "0.3.0", "0.3.1-alpha.1"] {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.minimum_app_version = minimum_app_version.to_string();

        validate_manifest(&manifest).expect("manifest minimum should be compatible");
    }
}

#[test]
fn app_version_lifecycle_orders_dev_before_alpha_beta_and_rc() {
    for running in ["0.4.0-alpha.1", "0.4.0-beta.1", "0.4.0-rc.1", "0.4.0"] {
        validate_app_version_compatibility_with_running("0.4.0-dev.1", running)
            .expect("later lifecycle should accept dev rules");
    }

    assert!(
        validate_app_version_compatibility_with_running("0.4.0-alpha.1", "0.4.0-dev.1").is_err()
    );
}

#[test]
fn validation_rejects_invalid_running_app_version_without_panicking() {
    assert_error_kind(
        validate_app_version_compatibility_with_running("0.4.0-alpha", "development-build"),
        ResolveError::InvalidRunningAppVersion("development-build".to_string()),
    );
}

#[test]
fn validation_rejects_invalid_artifact_definitions() {
    let mut empty_id = builtin_manifest().expect("manifest");
    empty_id.artifacts[0].id = String::new();
    assert_error_kind(
        validate_manifest(&empty_id),
        ResolveError::MissingArtifactId,
    );

    let mut duplicate_id = builtin_manifest().expect("manifest");
    duplicate_id
        .artifacts
        .push(duplicate_id.artifacts[0].clone());
    assert_error_kind(
        validate_manifest(&duplicate_id),
        ResolveError::DuplicateArtifactId("sodium".to_string()),
    );

    let mut missing_project = builtin_manifest().expect("manifest");
    missing_project.artifacts[0].source.project_id = String::new();
    assert_error_kind(
        validate_manifest(&missing_project),
        ResolveError::MissingArtifactProjectId("sodium".to_string()),
    );

    let mut missing_slug = builtin_manifest().expect("manifest");
    missing_slug.artifacts[0].source.slug = String::new();
    assert_error_kind(
        validate_manifest(&missing_slug),
        ResolveError::MissingArtifactSlug("sodium".to_string()),
    );

    let mut user_owned = builtin_manifest().expect("manifest");
    user_owned.artifacts[0].ownership_class = OwnershipClass::UserManaged;
    assert_error_kind(
        validate_manifest(&user_owned),
        ResolveError::InvalidArtifactOwnership("sodium".to_string()),
    );
}

#[test]
fn validation_rejects_noncanonical_artifact_provider_identities() {
    for value in ["AANobbM", "AANobbMI9", "AANobbM-", " AANobbM"] {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.artifacts[0].source.project_id = value.to_string();
        assert!(matches!(
            validate_manifest(&manifest),
            Err(ResolveError::ManifestBound("artifact Modrinth project id"))
        ));
    }

    for value in [" sodium", "sodium "] {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.artifacts[0].source.slug = value.to_string();
        assert!(matches!(
            validate_manifest(&manifest),
            Err(ResolveError::ManifestBound("artifact slug padding"))
        ));
    }
}

#[test]
fn manifest_rejects_unverifiable_artifact_publisher_signature_fields() {
    let error = serde_json::from_value::<Manifest>(serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "minimum_app_version": "0.4.0-alpha",
        "rule_channel": "bundled",
        "artifacts": [{
            "id": "sodium",
            "type": "mod",
            "source": {
                "provider": "modrinth",
                "project_id": "sodium",
                "slug": "sodium"
            },
            "checksum_policy": "provider_sha512",
            "ownership_class": "composition_managed",
            "publisher_signature": {
                "algorithm": "ed25519",
                "signature": "00"
            }
        }],
        "compositions": [],
        "emergency_disables": []
    }))
    .expect_err("unmodeled artifact signature fields should be invalid current schema");

    assert!(error.to_string().contains("publisher_signature"));
}

#[test]
fn validation_rejects_invalid_managed_mod_artifact_references() {
    let mut missing_reference = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut missing_reference).artifact_id = String::new();
    assert_error_kind(
        validate_manifest(&missing_reference),
        ResolveError::MissingManagedModArtifactId,
    );

    let mut unknown_reference = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut unknown_reference).artifact_id = "missing".to_string();
    assert_error_kind(
        validate_manifest(&unknown_reference),
        ResolveError::UnknownManagedModArtifact("missing".to_string()),
    );

    let mut project_mismatch = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut project_mismatch).project_id = "gvQqBUqZ".to_string();
    assert_error_kind(
        validate_manifest(&project_mismatch),
        ResolveError::ManagedModProjectMismatch {
            artifact_id: "sodium".to_string(),
            expected: "AANobbMI".to_string(),
            actual: "gvQqBUqZ".to_string(),
        },
    );

    let mut slug_mismatch = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut slug_mismatch).slug = "other-slug".to_string();
    assert_error_kind(
        validate_manifest(&slug_mismatch),
        ResolveError::ManagedModSlugMismatch {
            artifact_id: "sodium".to_string(),
            expected: "sodium".to_string(),
            actual: "other-slug".to_string(),
        },
    );
}

#[test]
fn validation_rejects_noncanonical_managed_mod_provider_identities() {
    for value in ["AANobbM", "AANobbMI9", "AANobbM-", " AANobbM"] {
        let mut manifest = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut manifest).project_id = value.to_string();
        assert!(matches!(
            validate_manifest(&manifest),
            Err(ResolveError::ManifestBound(
                "managed mod Modrinth project id"
            ))
        ));
    }

    for value in [" sodium", "sodium "] {
        let mut manifest = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut manifest).slug = value.to_string();
        assert!(matches!(
            validate_manifest(&manifest),
            Err(ResolveError::ManifestBound("managed mod slug padding"))
        ));
    }
}

#[test]
fn validation_accepts_builtin_manifest_and_valid_managed_mod_version_ranges() {
    let manifest = builtin_manifest().expect("manifest");
    validate_manifest(&manifest).expect("built-in manifest should validate");

    for version_range in ["", ">=1.16 <1.19.4", ">=1.20.1", "1.20.4", "24w14a"] {
        let mut manifest = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut manifest).version_range = version_range.to_string();

        validate_manifest(&manifest).expect("managed mod version_range should validate");
    }
}

#[test]
fn validation_rejects_malformed_managed_mod_version_ranges() {
    for version_range in [">=", ">=not-a-version"] {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            managed_mod.version_range = version_range.to_string();
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModVersionRange {
                artifact_id,
                version_range: version_range.to_string(),
            },
        );
    }
}

#[test]
fn validation_accepts_bounded_exact_managed_mod_game_versions() {
    let mut manifest = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut manifest).exact_game_versions =
        vec!["1.21.4".to_string(), "26.1.2".to_string()];

    validate_manifest(&manifest).expect("exact game versions should validate");
}

#[test]
fn validation_rejects_conflicting_managed_mod_version_selectors() {
    let mut manifest = builtin_manifest().expect("manifest");
    let artifact_id = {
        let managed_mod = first_managed_mod_mut(&mut manifest);
        managed_mod.version_range = ">=1.20".to_string();
        managed_mod.exact_game_versions = vec!["1.21.4".to_string()];
        managed_mod.artifact_id.clone()
    };

    assert_error_kind(
        validate_manifest(&manifest),
        ResolveError::ConflictingManagedModVersionSelectors { artifact_id },
    );
}

#[test]
fn validation_rejects_invalid_exact_managed_mod_game_versions() {
    for game_version in ["", " 1.21.4", "1.21.4 ", "not-a-version"] {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            managed_mod.exact_game_versions = vec![game_version.to_string()];
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModExactGameVersion {
                artifact_id,
                game_version: game_version.to_string(),
            },
        );
    }
}

#[test]
fn validation_rejects_control_and_oversized_exact_managed_mod_game_versions() {
    for (game_version, expected_field) in [
        ("1.21.4\n".to_string(), "managed mod exact game version"),
        ("1".repeat(65), "managed mod exact game version"),
    ] {
        let mut manifest = builtin_manifest().expect("manifest");
        first_managed_mod_mut(&mut manifest).exact_game_versions = vec![game_version];

        assert!(matches!(
            validate_manifest(&manifest),
            Err(ResolveError::ManifestBound(field)) if field == expected_field
        ));
    }
}

#[test]
fn validation_rejects_duplicate_and_oversized_exact_game_version_lists() {
    let mut duplicate = builtin_manifest().expect("manifest");
    let artifact_id = {
        let managed_mod = first_managed_mod_mut(&mut duplicate);
        managed_mod.exact_game_versions = vec!["1.21.4".to_string(), "1.21.4".to_string()];
        managed_mod.artifact_id.clone()
    };
    assert_error_kind(
        validate_manifest(&duplicate),
        ResolveError::DuplicateManagedModExactGameVersion {
            artifact_id,
            game_version: "1.21.4".to_string(),
        },
    );

    let mut oversized = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut oversized).exact_game_versions =
        (0..65).map(|patch| format!("1.20.{patch}")).collect();
    assert!(matches!(
        validate_manifest(&oversized),
        Err(ResolveError::ManifestBound(
            "managed mod exact game version count"
        ))
    ));
}

#[test]
fn validation_accepts_default_and_valid_managed_mod_hardware_requirements() {
    let mut default_manifest = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut default_manifest).hardware_req =
        Some(HardwareRequirement::default());
    validate_manifest(&default_manifest).expect("default hardware_req should validate");

    let mut nvidium_manifest = builtin_manifest().expect("manifest");
    let nvidium = nvidium_managed_mod_mut(&mut nvidium_manifest);
    nvidium.hardware_req = Some(HardwareRequirement {
        gpu_vendor: "nvidia".to_string(),
        gpu_arch_min: 2,
        ..HardwareRequirement::default()
    });
    validate_manifest(&nvidium_manifest).expect("Nvidium hardware_req should validate");
}

#[test]
fn validation_rejects_negative_managed_mod_hardware_requirements() {
    for (field, value) in [
        ("gpu_arch_min", -1),
        ("min_ram_mb", -2048),
        ("min_cores", -2),
    ] {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            let mut requirement = HardwareRequirement::default();
            match field {
                "gpu_arch_min" => requirement.gpu_arch_min = value,
                "min_ram_mb" => requirement.min_ram_mb = value,
                "min_cores" => requirement.min_cores = value,
                _ => unreachable!("test field should be covered"),
            }
            managed_mod.hardware_req = Some(requirement);
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModHardwareRequirement {
                artifact_id,
                field,
                value,
            },
        );
    }
}

#[test]
fn validation_rejects_oversized_managed_mod_gpu_vendor() {
    let mut manifest = builtin_manifest().expect("manifest");
    first_managed_mod_mut(&mut manifest).hardware_req = Some(HardwareRequirement {
        gpu_vendor: "x".repeat(65),
        ..HardwareRequirement::default()
    });

    assert!(matches!(
        validate_manifest(&manifest),
        Err(ResolveError::ManifestBound(
            "managed mod hardware gpu vendor"
        ))
    ));
}

#[test]
fn validation_accepts_builtin_manifest_and_undeclared_mutual_exclusions() {
    let mut manifest = builtin_manifest().expect("manifest");
    let nvidium = nvidium_managed_mod_mut(&mut manifest);
    assert_eq!(nvidium.mutual_exclusions, vec!["iris".to_string()]);
    nvidium.mutual_exclusions.push("sodium-extra".to_string());

    validate_manifest(&manifest).expect("mutual exclusions need not be managed artifacts");
}

#[test]
fn validation_rejects_blank_managed_mod_mutual_exclusions() {
    for exclusion in ["", " \t "] {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            managed_mod.mutual_exclusions = vec![exclusion.to_string()];
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModMutualExclusion {
                artifact_id,
                field: "entry",
                value: exclusion.to_string(),
            },
        );
    }
}

#[test]
fn validation_rejects_whitespace_padded_managed_mod_mutual_exclusions() {
    for exclusion in [" iris", "iris "] {
        let mut manifest = builtin_manifest().expect("manifest");
        let artifact_id = {
            let managed_mod = first_managed_mod_mut(&mut manifest);
            managed_mod.mutual_exclusions = vec![exclusion.to_string()];
            managed_mod.artifact_id.clone()
        };

        assert_error_kind(
            validate_manifest(&manifest),
            ResolveError::InvalidManagedModMutualExclusion {
                artifact_id,
                field: "entry",
                value: exclusion.to_string(),
            },
        );
    }
}

#[test]
fn validation_rejects_duplicate_managed_mod_mutual_exclusions_case_insensitively() {
    let mut manifest = builtin_manifest().expect("manifest");
    let artifact_id = {
        let managed_mod = first_managed_mod_mut(&mut manifest);
        managed_mod.mutual_exclusions = vec!["iris".to_string(), "IRIS".to_string()];
        managed_mod.artifact_id.clone()
    };

    assert_error_kind(
        validate_manifest(&manifest),
        ResolveError::InvalidManagedModMutualExclusion {
            artifact_id,
            field: "duplicate",
            value: "IRIS".to_string(),
        },
    );
}

#[test]
fn validation_rejects_invalid_emergency_disables() {
    for (disable, expected) in [
        (
            EmergencyDisable {
                id: String::new(),
                target: EmergencyDisableTarget::Composition,
                target_id: "family-f-fabric-extended".to_string(),
                reason: "Temporary hold.".to_string(),
                families: Vec::new(),
                loaders: Vec::new(),
                tiers: Vec::new(),
            },
            ResolveError::MissingEmergencyDisableId,
        ),
        (
            EmergencyDisable {
                id: "missing-target".to_string(),
                target: EmergencyDisableTarget::Composition,
                target_id: String::new(),
                reason: "Temporary hold.".to_string(),
                families: Vec::new(),
                loaders: Vec::new(),
                tiers: Vec::new(),
            },
            ResolveError::MissingEmergencyDisableTargetId,
        ),
        (
            EmergencyDisable {
                id: "missing-reason".to_string(),
                target: EmergencyDisableTarget::Composition,
                target_id: "family-f-fabric-extended".to_string(),
                reason: String::new(),
                families: Vec::new(),
                loaders: Vec::new(),
                tiers: Vec::new(),
            },
            ResolveError::MissingEmergencyDisableReason,
        ),
    ] {
        let mut manifest = builtin_manifest().expect("manifest");
        manifest.emergency_disables.push(disable);

        assert_error_kind(validate_manifest(&manifest), expected);
    }
}

#[test]
fn validation_rejects_duplicate_emergency_disable_ids() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.emergency_disables.push(test_composition_disable(
        "duplicate",
        "family-f-fabric-extended",
    ));
    manifest
        .emergency_disables
        .push(test_artifact_disable("duplicate", "sodium"));

    assert_error_kind(
        validate_manifest(&manifest),
        ResolveError::DuplicateEmergencyDisableId("duplicate".to_string()),
    );
}

#[test]
fn validation_rejects_unknown_emergency_disable_targets() {
    let mut composition_manifest = builtin_manifest().expect("manifest");
    composition_manifest
        .emergency_disables
        .push(test_composition_disable(
            "unknown-composition",
            "missing-composition",
        ));
    assert_error_kind(
        validate_manifest(&composition_manifest),
        ResolveError::UnknownEmergencyDisableComposition("missing-composition".to_string()),
    );

    let mut artifact_manifest = builtin_manifest().expect("manifest");
    artifact_manifest
        .emergency_disables
        .push(test_artifact_disable(
            "unknown-artifact",
            "missing-artifact",
        ));
    assert_error_kind(
        validate_manifest(&artifact_manifest),
        ResolveError::UnknownEmergencyDisableArtifact("missing-artifact".to_string()),
    );
}

#[test]
fn disabled_composition_falls_back_to_next_eligible_tier() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.emergency_disables.push(test_composition_disable(
        "hold-family-f-extended",
        "family-f-fabric-extended",
    ));

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.20.4".to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-f-fabric-core");
    assert_eq!(plan.tier, CompositionTier::Core);
    for slug in FAMILY_F_FABRIC_CORE_ADDITIONS {
        assert_eq!(count_mods_with_slug(&plan.mods, slug), 1, "{slug}");
    }
    assert_eq!(
        plan.fallback_reason,
        "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option."
    );
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("family-f-fabric-extended skipped by emergency disable")
            && warning.contains("Temporary hold.")
    }));
}

#[test]
fn depleted_higher_bundle_falls_back_with_compatibility_reason() {
    let mut manifest = builtin_manifest().expect("manifest");
    let extended = manifest
        .compositions
        .iter_mut()
        .find(|composition| composition.id == "family-f-fabric-extended")
        .expect("extended composition");
    extended.mods = vec![
        ManagedMod {
            artifact_id: "nvidium".to_string(),
            project_id: "SfMw2IZN".to_string(),
            slug: "nvidium".to_string(),
            name: "Nvidium".to_string(),
            condition: ModCondition::Hardware,
            version_range: ">=1.20.1".to_string(),
            exact_game_versions: Vec::new(),
            hardware_req: Some(HardwareRequirement {
                gpu_vendor: "nvidia".to_string(),
                gpu_arch_min: 2,
                min_ram_mb: 0,
                min_cores: 0,
            }),
            mutual_exclusions: Vec::new(),
        },
        ManagedMod {
            artifact_id: "sodium".to_string(),
            project_id: "AANobbMI".to_string(),
            slug: "sodium".to_string(),
            name: "Sodium".to_string(),
            condition: ModCondition::Recommend,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        },
    ];

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.20.4".to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-f-fabric-core");
    assert_eq!(plan.tier, CompositionTier::Core);
    assert_eq!(
        plan.fallback_reason,
        "A faster performance bundle is not compatible with this instance, so Axial chose a safer option."
    );
    assert!(plan.warnings.iter().any(|warning| {
        warning
            == "family-f-fabric-extended skipped: not enough compatible performance mods for this instance"
    }));
}

#[test]
fn artifact_disable_drops_matching_managed_mod_with_warning() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest
        .emergency_disables
        .push(test_artifact_disable("hold-sodium", "sodium"));

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.20.4".to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert_eq!(plan.composition_id, "family-f-fabric-extended");
    assert!(
        plan.mods
            .iter()
            .all(|managed_mod| managed_mod.slug != "sodium")
    );
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("sodium skipped by emergency disable hold-sodium")
            && warning.contains("Temporary hold.")
    }));
}

#[test]
fn artifact_disable_targets_declared_immutable_project_id() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.artifacts[0].id = "sodium-artifact".to_string();
    for composition in &mut manifest.compositions {
        for managed_mod in &mut composition.mods {
            if managed_mod.artifact_id == "sodium" {
                managed_mod.artifact_id = "sodium-artifact".to_string();
            }
        }
    }
    manifest
        .emergency_disables
        .push(test_artifact_disable("hold-sodium-project", "AANobbMI"));
    validate_manifest(&manifest).expect("declared project identity should validate");

    let plan = resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: "1.20.4".to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware: HardwareProfile::default(),
            installed_mods: Vec::new(),
        },
    );

    assert!(
        plan.mods
            .iter()
            .all(|managed_mod| managed_mod.slug != "sodium")
    );
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("sodium skipped by emergency disable hold-sodium-project")
    }));
}

#[test]
fn validation_rejects_emergency_disable_slug_alias() {
    let mut manifest = builtin_manifest().expect("manifest");
    manifest.artifacts[0].id = "sodium-artifact".to_string();
    for composition in &mut manifest.compositions {
        for managed_mod in &mut composition.mods {
            if managed_mod.artifact_id == "sodium" {
                managed_mod.artifact_id = "sodium-artifact".to_string();
            }
        }
    }
    manifest
        .emergency_disables
        .push(test_artifact_disable("hold-sodium-slug", "sodium"));

    assert_error_kind(
        validate_manifest(&manifest),
        ResolveError::UnknownEmergencyDisableArtifact("sodium".to_string()),
    );
}

fn test_composition_disable(id: &str, target_id: &str) -> EmergencyDisable {
    EmergencyDisable {
        id: id.to_string(),
        target: EmergencyDisableTarget::Composition,
        target_id: target_id.to_string(),
        reason: "Temporary hold.".to_string(),
        families: Vec::new(),
        loaders: Vec::new(),
        tiers: Vec::new(),
    }
}

fn test_artifact_disable(id: &str, target_id: &str) -> EmergencyDisable {
    EmergencyDisable {
        id: id.to_string(),
        target: EmergencyDisableTarget::Artifact,
        target_id: target_id.to_string(),
        reason: "Temporary hold.".to_string(),
        families: Vec::new(),
        loaders: Vec::new(),
        tiers: Vec::new(),
    }
}

fn first_managed_mod_mut(manifest: &mut Manifest) -> &mut ManagedMod {
    manifest
        .compositions
        .iter_mut()
        .find_map(|composition| composition.mods.first_mut())
        .expect("test manifest should include a managed mod")
}

fn nvidium_managed_mod_mut(manifest: &mut Manifest) -> &mut ManagedMod {
    manifest
        .compositions
        .iter_mut()
        .flat_map(|composition| &mut composition.mods)
        .find(|managed_mod| managed_mod.slug == "nvidium")
        .expect("test manifest should include nvidium")
}

fn count_mods_with_slug(mods: &[ManagedMod], slug: &str) -> usize {
    mods.iter()
        .filter(|managed_mod| managed_mod.slug == slug)
        .count()
}

fn fabric_plan(game_version: &str, hardware: HardwareProfile) -> CompositionPlan {
    let manifest = builtin_manifest().expect("manifest");
    resolve_plan(
        Some(&manifest),
        ResolutionRequest {
            game_version: game_version.to_string(),
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            hardware,
            installed_mods: Vec::new(),
        },
    )
}

fn nvidia_turing_hardware() -> HardwareProfile {
    HardwareProfile {
        gpu_vendor: "nvidia".to_string(),
        gpu_arch: 2,
        ..HardwareProfile::default()
    }
}

fn assert_error_kind(result: Result<(), ResolveError>, expected: ResolveError) {
    let error = result.expect_err("manifest should be invalid");
    assert_eq!(
        std::mem::discriminant(&error),
        std::mem::discriminant(&expected)
    );
    assert_eq!(error.to_string(), expected.to_string());
}
