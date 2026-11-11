use super::app_version::validate_app_version_compatibility_with_running;
use super::hardware::{
    decode_windows_command_output, gpu_vendor_from_model_name, gpu_vendor_from_pci_id,
    is_drm_card_path, nvidia_arch_from_model, nvidia_model_from_information,
    parse_windows_gpu_names, select_gpu_from_names, select_gpu_vendor_from_vendors,
};
use super::{
    PERFORMANCE_MANIFEST_SCHEMA_VERSION, ResolveError, builtin_manifest, resolve_plan,
    validate_manifest,
};
use crate::performance::model::{
    CompositionPlan, CompositionTier, EmergencyDisable, EmergencyDisableTarget, HardwareProfile,
    HardwareRequirement, ManagedMod, Manifest, OwnershipClass, PerformanceMode, ResolutionRequest,
    VersionFamily,
};
use std::path::Path;

const FAMILY_F_FABRIC_CORE_ADDITIONS: &[&str] = &[
    "scalablelux",
    "particle-core",
    "threadtweak",
    "badoptimizations",
];
const HELD: &str = "A faster performance bundle is temporarily unavailable, so Axial chose the safest available option.";
const NO_TURING: &str = "nvidium skipped: no NVIDIA Turing+ GPU detected";

fn manifest() -> Manifest {
    builtin_manifest().expect("manifest")
}

fn resolve(
    manifest: &Manifest,
    game_version: &str,
    loader: &str,
    hardware: HardwareProfile,
    installed_mods: &[&str],
) -> CompositionPlan {
    resolve_plan(
        Some(manifest),
        ResolutionRequest {
            game_version: game_version.to_string(),
            loader: loader.to_string(),
            mode: PerformanceMode::Managed,
            hardware,
            installed_mods: installed_mods.iter().map(ToString::to_string).collect(),
        },
    )
}

fn count(plan: &CompositionPlan, slug: &str) -> usize {
    plan.mods
        .iter()
        .filter(|managed| managed.slug == slug)
        .count()
}

fn gpu(vendor: &str, arch: i32) -> HardwareProfile {
    HardwareProfile {
        gpu_vendor: vendor.to_string(),
        gpu_arch: arch,
        ..HardwareProfile::default()
    }
}

fn disable(target: EmergencyDisableTarget, id: &str, target_id: &str) -> EmergencyDisable {
    EmergencyDisable {
        id: id.to_string(),
        target,
        target_id: target_id.to_string(),
        reason: "Temporary hold.".to_string(),
        families: Vec::new(),
        loaders: Vec::new(),
        tiers: Vec::new(),
    }
}

fn first_mod(manifest: &mut Manifest) -> &mut ManagedMod {
    manifest
        .compositions
        .iter_mut()
        .find_map(|composition| composition.mods.first_mut())
        .expect("test manifest should include a managed mod")
}

fn nvidium(manifest: &mut Manifest) -> &mut ManagedMod {
    manifest
        .compositions
        .iter_mut()
        .flat_map(|composition| &mut composition.mods)
        .find(|managed| managed.slug == "nvidium")
        .expect("test manifest should include nvidium")
}

fn rename_sodium_artifact(manifest: &mut Manifest) {
    manifest.artifacts[0].id = "sodium-artifact".to_string();
    for managed in manifest
        .compositions
        .iter_mut()
        .flat_map(|composition| &mut composition.mods)
        .filter(|managed| managed.artifact_id == "sodium")
    {
        managed.artifact_id = "sodium-artifact".to_string();
    }
}

fn rejects(mutate: impl FnOnce(&mut Manifest), expected: ResolveError) {
    let mut manifest = manifest();
    mutate(&mut manifest);
    let error = validate_manifest(&manifest).expect_err("manifest should be invalid");
    assert_eq!(
        std::mem::discriminant(&error),
        std::mem::discriminant(&expected)
    );
    assert_eq!(error.to_string(), expected.to_string());
}

fn accepts(mutate: impl FnOnce(&mut Manifest)) {
    let mut manifest = manifest();
    mutate(&mut manifest);
    validate_manifest(&manifest).expect("manifest should validate");
}

#[test]
fn managed_plans_select_the_named_composition_for_each_family_and_loader() {
    use CompositionTier::{Core, Extended, VanillaEnhanced};
    let all = &["vanilla", "fabric", "forge", "neoforge", "quilt"][..];
    let manifest = manifest();
    for (game_version, loaders, family, composition_id, tier) in [
        (
            "1.5.2",
            all,
            VersionFamily::A,
            "family-a-vanilla-enhanced",
            VanillaEnhanced,
        ),
        (
            "1.7.10",
            all,
            VersionFamily::B,
            "family-b-vanilla-enhanced",
            VanillaEnhanced,
        ),
        (
            "1.15.2",
            all,
            VersionFamily::D,
            "family-d-vanilla-enhanced",
            VanillaEnhanced,
        ),
        (
            "1.12.2",
            &["vanilla", "fabric", "neoforge", "quilt"][..],
            VersionFamily::C,
            "family-c-vanilla-enhanced",
            VanillaEnhanced,
        ),
        (
            "1.12.2",
            &["forge"],
            VersionFamily::C,
            "family-c-forge-core",
            Core,
        ),
        (
            "1.16.5",
            &["fabric"],
            VersionFamily::E,
            "family-e-fabric-extended",
            Extended,
        ),
        (
            "1.20.1",
            &["fabric"],
            VersionFamily::E,
            "family-e-fabric-extended",
            Extended,
        ),
        (
            "1.20.4",
            &["fabric"],
            VersionFamily::F,
            "family-f-fabric-extended",
            Extended,
        ),
        (
            "1.20.1",
            &["forge", "neoforge"],
            VersionFamily::E,
            "family-e-forge-extended",
            Extended,
        ),
        (
            "1.20.4",
            &["forge", "neoforge"],
            VersionFamily::F,
            "family-f-forge-extended",
            Extended,
        ),
    ] {
        for loader in loaders {
            let plan = resolve(
                &manifest,
                game_version,
                loader,
                HardwareProfile::default(),
                &[],
            );
            let case = format!("{game_version} {loader}");
            assert_eq!(plan.composition_id, composition_id, "{case}");
            assert_eq!(plan.family, family, "{case}");
            assert_eq!(plan.loader, *loader, "{case}");
            assert_eq!(plan.tier, tier, "{case}");
            assert_eq!(plan.mods.is_empty(), tier == VanillaEnhanced, "{case}");
            assert!(plan.fallback_reason.is_empty(), "{case}");
        }
    }
}

#[test]
fn version_gated_mods_are_included_once_only_for_supported_game_versions() {
    let manifest = manifest();
    let older_fabric = ["lazydfu", "smooth-boot-reloaded", "starlight"];
    for (game_version, loader, included, excluded) in [
        (
            "1.12.2",
            "forge",
            &["foamfix", "ai-improvements", "clumps"][..],
            &[][..],
        ),
        ("1.16.1", "fabric", &["memoryleakfix"], &["ebe"]),
        ("1.16.2", "fabric", &["ebe", "memoryleakfix"], &[]),
        ("1.16.4", "fabric", &["modernfix"], &[]),
        ("1.16.5", "fabric", &older_fabric, &[]),
        ("1.20", "fabric", &["moreculling"], &["cloth-config"]),
        (
            "1.20.1",
            "fabric",
            &["sodium", "moreculling", "cloth-config"],
            &[&older_fabric[..], FAMILY_F_FABRIC_CORE_ADDITIONS].concat(),
        ),
        ("1.20.1", "forge", &["embeddium", "ferrite-core"], &[]),
        ("1.20.1", "neoforge", &["embeddium", "ferrite-core"], &[]),
        (
            "1.20.4",
            "fabric",
            &[
                &["sodium", "ebe", "memoryleakfix", "modernfix"][..],
                FAMILY_F_FABRIC_CORE_ADDITIONS,
            ]
            .concat(),
            &[],
        ),
        ("1.20.4", "forge", &["embeddium", "ferrite-core"], &[]),
        ("1.20.4", "neoforge", &["embeddium", "ferrite-core"], &[]),
        ("1.21.1", "fabric", &["modernfix"], &[]),
        (
            "1.21.4",
            "fabric",
            &["ebe", "modernfix"],
            &["memoryleakfix"],
        ),
        ("1.21.5", "fabric", &[], &["ebe", "memoryleakfix"]),
        (
            "1.21.11",
            "fabric",
            &[],
            &["ebe", "memoryleakfix", "modernfix"],
        ),
        ("1.21.11", "forge", &[], &["ebe", "memoryleakfix"]),
        ("1.21.11", "neoforge", &[], &["ebe", "memoryleakfix"]),
        ("26.1", "fabric", &["modernfix"], &[]),
        ("26.2", "fabric", &[], &["modernfix"]),
    ] {
        let plan = resolve(
            &manifest,
            game_version,
            loader,
            HardwareProfile::default(),
            &[],
        );
        for slug in included {
            assert_eq!(count(&plan, slug), 1, "{game_version} {loader} {slug}");
        }
        for slug in excluded {
            assert_eq!(count(&plan, slug), 0, "{game_version} {loader} {slug}");
        }
    }
}

#[test]
fn composition_hold_falls_back_to_the_next_eligible_tier() {
    for (game_version, loader, held, composition_id, tier, included, excluded) in [
        (
            "1.12.2",
            "forge",
            "family-c-forge-core",
            "family-c-vanilla-enhanced",
            CompositionTier::VanillaEnhanced,
            &[][..],
            &["foamfix"][..],
        ),
        (
            "1.20.1",
            "forge",
            "family-e-forge-extended",
            "family-e-forge-core",
            CompositionTier::Core,
            &["embeddium", "ferrite-core"],
            &[],
        ),
        (
            "1.20.4",
            "neoforge",
            "family-f-forge-extended",
            "family-f-forge-core",
            CompositionTier::Core,
            &["embeddium", "ferrite-core"],
            &[],
        ),
        (
            "1.20.1",
            "fabric",
            "family-e-fabric-extended",
            "family-e-fabric-core",
            CompositionTier::Core,
            &[],
            FAMILY_F_FABRIC_CORE_ADDITIONS,
        ),
        (
            "1.20.4",
            "fabric",
            "family-f-fabric-extended",
            "family-f-fabric-core",
            CompositionTier::Core,
            FAMILY_F_FABRIC_CORE_ADDITIONS,
            &[],
        ),
    ] {
        let mut manifest = manifest();
        manifest.emergency_disables.push(disable(
            EmergencyDisableTarget::Composition,
            "hold",
            held,
        ));
        let plan = resolve(
            &manifest,
            game_version,
            loader,
            HardwareProfile::default(),
            &[],
        );

        assert_eq!(plan.composition_id, composition_id, "{held}");
        assert_eq!(plan.tier, tier, "{held}");
        for slug in included {
            assert_eq!(count(&plan, slug), 1, "{held} {slug}");
        }
        for slug in excluded {
            assert_eq!(count(&plan, slug), 0, "{held} {slug}");
        }
        assert_eq!(plan.fallback_reason, HELD);
        assert!(plan.warnings.iter().any(|warning| {
            warning.contains(&format!("{held} skipped by emergency disable"))
                && warning.contains("Temporary hold.")
        }));
    }
}

#[test]
fn depleted_higher_bundle_falls_back_with_compatibility_reason() {
    let mut manifest = manifest();
    manifest
        .compositions
        .iter_mut()
        .find(|composition| composition.id == "family-f-fabric-extended")
        .expect("extended composition")
        .mods
        .retain(|managed| managed.slug == "nvidium" || managed.slug == "sodium");

    let plan = resolve(
        &manifest,
        "1.20.4",
        "fabric",
        HardwareProfile::default(),
        &[],
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
fn nvidium_requires_nvidia_turing_or_newer_and_no_iris() {
    let manifest = manifest();
    let iris = "nvidium skipped: incompatible with managed mod iris";
    for game_version in ["1.20.1", "1.20.4"] {
        for (hardware, installed, included, warning) in [
            (gpu("nvidia", 2), &[][..], true, None),
            (gpu("nvidia", 3), &[], true, None),
            (gpu("nvidia", 1), &[], false, Some(NO_TURING)),
            (gpu("nvidia", 0), &[], false, Some(NO_TURING)),
            (gpu("amd", 0), &[], false, Some(NO_TURING)),
            (gpu("nvidia", 2), &["iris"], false, Some(iris)),
        ] {
            let plan = resolve(&manifest, game_version, "fabric", hardware, installed);
            assert_eq!(plan.tier, CompositionTier::Extended);
            assert_eq!(
                count(&plan, "nvidium"),
                usize::from(included),
                "{game_version}"
            );
            assert_eq!(
                plan.warnings.iter().any(|w| w == NO_TURING),
                warning == Some(NO_TURING),
                "{game_version}"
            );
            if let Some(warning) = warning {
                assert!(plan.warnings.iter().any(|w| w == warning), "{game_version}");
            }
        }
    }
    let older = resolve(&manifest, "1.16.5", "fabric", gpu("nvidia", 2), &[]);
    assert_eq!(count(&older, "nvidium"), 0);
    assert!(!older.warnings.iter().any(|w| w == NO_TURING));
}

#[test]
fn artifact_hold_targets_the_declared_project_id_not_a_slug_alias() {
    let mut held = manifest();
    held.emergency_disables.push(disable(
        EmergencyDisableTarget::Artifact,
        "hold-sodium",
        "sodium",
    ));
    let plan = resolve(&held, "1.20.4", "fabric", HardwareProfile::default(), &[]);
    assert_eq!(plan.composition_id, "family-f-fabric-extended");
    assert_eq!(count(&plan, "sodium"), 0);
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("sodium skipped by emergency disable hold-sodium")
            && warning.contains("Temporary hold.")
    }));

    rejects(
        |manifest| {
            rename_sodium_artifact(manifest);
            manifest.emergency_disables.push(disable(
                EmergencyDisableTarget::Artifact,
                "hold-sodium-slug",
                "sodium",
            ));
        },
        ResolveError::UnknownEmergencyDisableArtifact("sodium".to_string()),
    );

    let mut renamed = manifest();
    rename_sodium_artifact(&mut renamed);
    renamed.emergency_disables.push(disable(
        EmergencyDisableTarget::Artifact,
        "hold-sodium-project",
        "AANobbMI",
    ));
    validate_manifest(&renamed).expect("declared project identity should validate");
    let plan = resolve(
        &renamed,
        "1.20.4",
        "fabric",
        HardwareProfile::default(),
        &[],
    );
    assert_eq!(count(&plan, "sodium"), 0);
    assert!(plan.warnings.iter().any(|warning| {
        warning.contains("sodium skipped by emergency disable hold-sodium-project")
    }));
}

#[test]
fn gpu_detection_maps_vendor_ids_model_names_and_architecture() {
    for (pci, vendor) in [
        ("0x10de\n", Some("nvidia")),
        ("0X1002", Some("amd")),
        ("0x1022", Some("amd")),
        (" 0x8086 ", Some("intel")),
        ("0x1234", None),
    ] {
        assert_eq!(gpu_vendor_from_pci_id(pci), vendor, "{pci:?}");
    }
    for (name, vendor) in [
        (
            "Advanced Micro Devices, Inc. Radeon RX 7800 XT",
            Some("amd"),
        ),
        ("Intel(R) UHD Graphics", Some("intel")),
        ("GeForce GTX 1660 SUPER", Some("nvidia")),
        ("Generic Display Adapter", None),
    ] {
        assert_eq!(gpu_vendor_from_model_name(name), vendor, "{name}");
    }
    for (vendors, selected) in [
        (&["intel"][..], "intel"),
        (&["intel", "amd"], "amd"),
        (&["intel", "amd", "nvidia"], "nvidia"),
        (&["unknown"], ""),
        (&[], ""),
    ] {
        assert_eq!(
            select_gpu_vendor_from_vendors(vendors.iter().copied()),
            selected
        );
    }
    for (model, arch) in [
        ("NVIDIA GeForce GTX 1070 Ti", 1),
        ("NVIDIA GeForce GTX 1660 Ti", 2),
        ("NVIDIA GeForce RTX 2070 SUPER", 2),
        ("NVIDIA GeForce RTX 3080", 3),
        ("NVIDIA GeForce RTX 4090", 4),
        ("NVIDIA GeForce RTX 5090", 4),
        ("NVIDIA Quadro RTX 5000", 2),
        ("NVIDIA RTX A5000", 3),
        ("NVIDIA Quadro P2000", 0),
        ("Unknown GPU", 0),
    ] {
        assert_eq!(nvidia_arch_from_model(model), arch, "{model}");
    }
    let names = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        select_gpu_from_names(&names(&["Intel(R) UHD Graphics", "AMD Radeon RX 7900 XT"])),
        ("amd".to_string(), 0)
    );
    assert_eq!(
        select_gpu_from_names(&names(&[
            "Intel(R) UHD Graphics",
            "NVIDIA GeForce RTX 4070",
            "AMD Radeon RX 7900 XT",
        ])),
        ("nvidia".to_string(), 4)
    );
    assert_eq!(
        select_gpu_from_names(&names(&["Generic Display Adapter"])),
        (String::new(), 0)
    );
}

#[test]
fn linux_gpu_sources_accept_only_card_nodes_and_model_lines() {
    for (path, accepted) in [
        ("/sys/class/drm/card0", true),
        ("/sys/class/drm/card12", true),
        ("/sys/class/drm/card", false),
        ("/sys/class/drm/card0-DP-1", false),
        ("/sys/class/drm/renderD128", false),
        ("/sys/class/drm/controlD64", false),
        ("/sys/class/drm/cardx", false),
    ] {
        assert_eq!(is_drm_card_path(Path::new(path)), accepted, "{path}");
    }
    for (information, model) in [
        (
            "Model: \t\t NVIDIA GeForce RTX 3080\nIRQ: 54\nGPU UUID: GPU-test\n",
            Some("NVIDIA GeForce RTX 3080"),
        ),
        (
            "irq: 54\n model : nvidia geforce rtx 4070 \n",
            Some("nvidia geforce rtx 4070"),
        ),
        ("Model:\t \n", None),
        ("IRQ: 54\n", None),
    ] {
        assert_eq!(
            nvidia_model_from_information(information).as_deref(),
            model,
            "{information:?}"
        );
    }
}

#[test]
fn windows_gpu_output_decodes_utf8_and_utf16le_and_skips_the_header() {
    let utf16 = |text: &str| {
        text.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    let output = "Name\r\n\r\n NVIDIA GeForce RTX 4070 \r\nIntel UHD\r\n";
    for bytes in [
        output.as_bytes().to_vec(),
        utf16(&format!("\u{feff}{output}")),
        utf16(output),
    ] {
        assert_eq!(
            decode_windows_command_output(&bytes).as_deref(),
            Some(output)
        );
    }
    assert_eq!(decode_windows_command_output(b""), None);
    assert_eq!(
        parse_windows_gpu_names(output),
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
fn current_schema_requires_every_field_and_rejects_unmodeled_signatures() {
    let base = serde_json::json!({
        "schema_version": 3,
        "generated_at": "2026-04-02T00:00:00Z",
        "minimum_app_version": "0.4.0-alpha",
        "rule_channel": "bundled",
        "artifacts": [{
            "id": "sodium",
            "type": "mod",
            "source": {"provider": "modrinth", "project_id": "AANobbMI", "slug": "sodium"},
            "checksum_policy": "provider_sha512",
            "ownership_class": "composition_managed"
        }],
        "compositions": [],
        "emergency_disables": []
    });
    serde_json::from_value::<Manifest>(base.clone()).expect("complete current schema");
    for field in [
        "emergency_disables",
        "artifacts",
        "minimum_app_version",
        "rule_channel",
    ] {
        let mut manifest = base.clone();
        manifest.as_object_mut().unwrap().remove(field).unwrap();
        let error = serde_json::from_value::<Manifest>(manifest).unwrap_err();
        assert!(error.to_string().contains(field), "{field}: {error}");
    }
    let mut signed = base;
    signed["artifacts"][0]["publisher_signature"] =
        serde_json::json!({"algorithm": "ed25519", "signature": "00"});
    let error = serde_json::from_value::<Manifest>(signed).unwrap_err();
    assert!(error.to_string().contains("publisher_signature"));
}

#[test]
fn validation_rejects_incompatible_metadata_and_invalid_artifacts() {
    rejects(
        |m| m.schema_version = PERFORMANCE_MANIFEST_SCHEMA_VERSION - 1,
        ResolveError::UnsupportedSchema,
    );
    rejects(
        |m| m.minimum_app_version = "0.4.0".to_string(),
        ResolveError::UnsupportedAppVersion {
            required: "0.4.0".to_string(),
            running: env!("CARGO_PKG_VERSION").to_string(),
        },
    );
    rejects(
        |m| m.minimum_app_version = "latest".to_string(),
        ResolveError::InvalidMinimumAppVersion("latest".to_string()),
    );
    rejects(
        |m| m.minimum_app_version.clear(),
        ResolveError::MissingMinimumAppVersion,
    );
    rejects(
        |m| m.rule_channel = "nightly".to_string(),
        ResolveError::UnsupportedRuleChannel("nightly".to_string()),
    );
    rejects(|m| m.rule_channel.clear(), ResolveError::MissingRuleChannel);
    rejects(
        |m| m.artifacts[0].id.clear(),
        ResolveError::MissingArtifactId,
    );
    rejects(
        |m| m.artifacts.push(m.artifacts[0].clone()),
        ResolveError::DuplicateArtifactId("sodium".to_string()),
    );
    rejects(
        |m| m.artifacts[0].source.project_id.clear(),
        ResolveError::MissingArtifactProjectId("sodium".to_string()),
    );
    rejects(
        |m| m.artifacts[0].source.slug.clear(),
        ResolveError::MissingArtifactSlug("sodium".to_string()),
    );
    rejects(
        |m| m.artifacts[0].ownership_class = OwnershipClass::UserManaged,
        ResolveError::InvalidArtifactOwnership("sodium".to_string()),
    );
    for value in ["AANobbM", "AANobbMI9", "AANobbM-", " AANobbM"] {
        rejects(
            |m| m.artifacts[0].source.project_id = value.to_string(),
            ResolveError::ManifestBound("artifact Modrinth project id"),
        );
    }
    for value in [" sodium", "sodium "] {
        rejects(
            |m| m.artifacts[0].source.slug = value.to_string(),
            ResolveError::ManifestBound("artifact slug padding"),
        );
    }
}

#[test]
fn validation_rejects_invalid_managed_mod_declarations() {
    let sodium = || "sodium".to_string();
    rejects(
        |m| first_mod(m).artifact_id.clear(),
        ResolveError::MissingManagedModArtifactId,
    );
    rejects(
        |m| first_mod(m).artifact_id = "missing".to_string(),
        ResolveError::UnknownManagedModArtifact("missing".to_string()),
    );
    rejects(
        |m| first_mod(m).project_id = "gvQqBUqZ".to_string(),
        ResolveError::ManagedModProjectMismatch {
            artifact_id: sodium(),
            expected: "AANobbMI".to_string(),
            actual: "gvQqBUqZ".to_string(),
        },
    );
    rejects(
        |m| first_mod(m).slug = "other-slug".to_string(),
        ResolveError::ManagedModSlugMismatch {
            artifact_id: sodium(),
            expected: sodium(),
            actual: "other-slug".to_string(),
        },
    );
    for value in ["AANobbM", "AANobbMI9", "AANobbM-", " AANobbM"] {
        rejects(
            |m| first_mod(m).project_id = value.to_string(),
            ResolveError::ManifestBound("managed mod Modrinth project id"),
        );
    }
    for value in [" sodium", "sodium "] {
        rejects(
            |m| first_mod(m).slug = value.to_string(),
            ResolveError::ManifestBound("managed mod slug padding"),
        );
    }
    for version_range in [">=", ">=not-a-version"] {
        rejects(
            |m| first_mod(m).version_range = version_range.to_string(),
            ResolveError::InvalidManagedModVersionRange {
                artifact_id: sodium(),
                version_range: version_range.to_string(),
            },
        );
    }
    rejects(
        |m| {
            first_mod(m).version_range = ">=1.20".to_string();
            first_mod(m).exact_game_versions = vec!["1.21.4".to_string()];
        },
        ResolveError::ConflictingManagedModVersionSelectors {
            artifact_id: sodium(),
        },
    );
    for game_version in ["", " 1.21.4", "1.21.4 ", "not-a-version"] {
        rejects(
            |m| first_mod(m).exact_game_versions = vec![game_version.to_string()],
            ResolveError::InvalidManagedModExactGameVersion {
                artifact_id: sodium(),
                game_version: game_version.to_string(),
            },
        );
    }
    for game_version in ["1.21.4\n".to_string(), "1".repeat(65)] {
        rejects(
            |m| first_mod(m).exact_game_versions = vec![game_version],
            ResolveError::ManifestBound("managed mod exact game version"),
        );
    }
    rejects(
        |m| first_mod(m).exact_game_versions = vec!["1.21.4".to_string(); 2],
        ResolveError::DuplicateManagedModExactGameVersion {
            artifact_id: sodium(),
            game_version: "1.21.4".to_string(),
        },
    );
    rejects(
        |m| {
            first_mod(m).exact_game_versions =
                (0..65).map(|patch| format!("1.20.{patch}")).collect()
        },
        ResolveError::ManifestBound("managed mod exact game version count"),
    );
    for (field, value, requirement) in [
        (
            "gpu_arch_min",
            -1,
            HardwareRequirement {
                gpu_arch_min: -1,
                ..HardwareRequirement::default()
            },
        ),
        (
            "min_ram_mb",
            -2048,
            HardwareRequirement {
                min_ram_mb: -2048,
                ..HardwareRequirement::default()
            },
        ),
        (
            "min_cores",
            -2,
            HardwareRequirement {
                min_cores: -2,
                ..HardwareRequirement::default()
            },
        ),
    ] {
        rejects(
            |m| first_mod(m).hardware_req = Some(requirement),
            ResolveError::InvalidManagedModHardwareRequirement {
                artifact_id: sodium(),
                field,
                value,
            },
        );
    }
    rejects(
        |m| {
            first_mod(m).hardware_req = Some(HardwareRequirement {
                gpu_vendor: "x".repeat(65),
                ..HardwareRequirement::default()
            })
        },
        ResolveError::ManifestBound("managed mod hardware gpu vendor"),
    );
    for exclusion in ["", " \t ", " iris", "iris "] {
        rejects(
            |m| first_mod(m).mutual_exclusions = vec![exclusion.to_string()],
            ResolveError::InvalidManagedModMutualExclusion {
                artifact_id: sodium(),
                field: "entry",
                value: exclusion.to_string(),
            },
        );
    }
    rejects(
        |m| first_mod(m).mutual_exclusions = vec!["iris".to_string(), "IRIS".to_string()],
        ResolveError::InvalidManagedModMutualExclusion {
            artifact_id: sodium(),
            field: "duplicate",
            value: "IRIS".to_string(),
        },
    );
}

#[test]
fn validation_rejects_invalid_emergency_disables() {
    let composition = |id: &str| {
        disable(
            EmergencyDisableTarget::Composition,
            id,
            "family-f-fabric-extended",
        )
    };
    for (mutate, expected) in [
        (
            (|d: &mut EmergencyDisable| d.id.clear()) as fn(&mut EmergencyDisable),
            ResolveError::MissingEmergencyDisableId,
        ),
        (
            |d| d.target_id.clear(),
            ResolveError::MissingEmergencyDisableTargetId,
        ),
        (
            |d| d.reason.clear(),
            ResolveError::MissingEmergencyDisableReason,
        ),
        (
            |d| d.target_id = "missing-composition".to_string(),
            ResolveError::UnknownEmergencyDisableComposition("missing-composition".to_string()),
        ),
    ] {
        let mut held = composition("hold");
        mutate(&mut held);
        rejects(|m| m.emergency_disables.push(held), expected);
    }
    rejects(
        |m| {
            m.emergency_disables.push(disable(
                EmergencyDisableTarget::Artifact,
                "hold",
                "missing-artifact",
            ))
        },
        ResolveError::UnknownEmergencyDisableArtifact("missing-artifact".to_string()),
    );
    rejects(
        |m| {
            m.emergency_disables.push(composition("duplicate"));
            m.emergency_disables.push(disable(
                EmergencyDisableTarget::Artifact,
                "duplicate",
                "sodium",
            ));
        },
        ResolveError::DuplicateEmergencyDisableId("duplicate".to_string()),
    );
}

#[test]
fn builtin_manifest_binds_all_managed_artifacts_to_immutable_provider_identities() {
    let manifest = manifest();
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
        ("cloth-config", "9s6osm5g"),
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
}

#[test]
fn validation_accepts_the_builtin_manifest_and_valid_variants() {
    accepts(|_| {});
    for minimum_app_version in [env!("CARGO_PKG_VERSION"), "0.3.0", "0.3.1-alpha.1"] {
        accepts(|m| m.minimum_app_version = minimum_app_version.to_string());
    }
    for version_range in ["", ">=1.16 <1.19.4", ">=1.20.1", "1.20.4", "24w14a"] {
        accepts(|m| first_mod(m).version_range = version_range.to_string());
    }
    accepts(|m| {
        first_mod(m).exact_game_versions = vec!["1.21.4".to_string(), "26.1.2".to_string()]
    });
    accepts(|m| first_mod(m).hardware_req = Some(HardwareRequirement::default()));
    accepts(|m| {
        nvidium(m).hardware_req = Some(HardwareRequirement {
            gpu_vendor: "nvidia".to_string(),
            gpu_arch_min: 2,
            ..HardwareRequirement::default()
        })
    });
    // Mutual exclusions name user mods and need not be managed artifacts.
    accepts(|m| {
        nvidium(m)
            .mutual_exclusions
            .push("sodium-extra".to_string())
    });
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
    let error = validate_app_version_compatibility_with_running("0.4.0-alpha", "development-build")
        .unwrap_err();
    assert!(matches!(
        error,
        ResolveError::InvalidRunningAppVersion(ref version) if version == "development-build"
    ));
}
