use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[path = "src/frontend_build_support.rs"]
mod frontend_build_support;
use frontend_build_support::{is_valid_frontend_relative_path, reset_frontend_destination};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationManifest {
    schema_version: u64,
    generation_id: String,
    document_entry: String,
    script_entry: String,
    files: Vec<GenerationFile>,
    graph: Vec<GenerationGraphOutput>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationGraphOutput {
    path: String,
    css_bundle: Option<String>,
    imports: Vec<GenerationGraphImport>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationGraphImport {
    path: String,
    dynamic: bool,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BundleMetrics {
    initial_javascript: u64,
    initial_css: u64,
    lazy_total: u64,
    public_assets: u64,
    largest_public_asset: u64,
    largest_generated_output: u64,
    generated_total: u64,
    packaged_payload: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BundleBudgetAuthority {
    schema_version: u64,
    maximum_bytes: BundleMetrics,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublicAssetAuthority {
    schema_version: u64,
    files: Vec<String>,
}

#[derive(Serialize)]
struct GenerationIdentity<'a> {
    schema_version: u64,
    document_entry: &'a str,
    script_entry: &'a str,
    files: &'a [GenerationFile],
    graph: &'a [GenerationGraphOutput],
}

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBEDDED_FRONTEND");
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let source = manifest_dir.join("../../frontend/dist");
    if env::var_os("CARGO_FEATURE_EMBEDDED_FRONTEND").is_none() {
        return;
    }
    let destination =
        PathBuf::from(env::var_os("OUT_DIR").expect("out dir")).join("embedded-frontend");
    reset_frontend_destination(&destination).expect("reset embedded frontend output");
    let source_metadata = fs::symlink_metadata(&source)
        .expect("embedded frontend generation is absent; run task frontend:build");
    assert!(
        source_metadata.is_dir() && !source_metadata.file_type().is_symlink(),
        "embedded frontend root must be a real directory"
    );
    let manifest_path = source.join("generation.json");
    let frontend_root = source.parent().expect("frontend root");
    let budget_path = frontend_root.join("bundle-budgets.json");
    let public_assets_path = frontend_root.join("public-assets.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!("cargo:rerun-if-changed={}", budget_path.display());
    println!("cargo:rerun-if-changed={}", public_assets_path.display());
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .expect("embedded frontend generation is absent; run task frontend:build");
    assert!(
        manifest_metadata.is_file() && !manifest_metadata.file_type().is_symlink(),
        "frontend generation manifest must be a real file"
    );
    let (manifest_bytes, manifest): (Vec<u8>, GenerationManifest) =
        read_canonical_json(&manifest_path, "frontend generation manifest");
    let (_, budget_authority): (Vec<u8>, BundleBudgetAuthority) =
        read_canonical_json(&budget_path, "frontend bundle budget authority");
    let (_, public_authority): (Vec<u8>, PublicAssetAuthority) =
        read_canonical_json(&public_assets_path, "frontend public asset authority");
    assert_eq!(
        manifest.schema_version, 1,
        "unsupported frontend generation schema"
    );
    assert_eq!(
        manifest.document_entry, "index.html",
        "invalid frontend document entry"
    );
    assert_eq!(
        manifest.script_entry, "app.js",
        "invalid frontend script entry"
    );
    assert_eq!(
        budget_authority.schema_version, 1,
        "invalid frontend budget schema"
    );
    assert_eq!(
        public_authority.schema_version, 1,
        "invalid public asset schema"
    );

    let mut previous: Option<&str> = None;
    let mut portable_paths = HashSet::new();
    for file in &manifest.files {
        validate_relative_path(&file.path);
        assert!(
            portable_paths.insert(file.path.to_ascii_lowercase()),
            "frontend generation contains a portable path collision"
        );
        if let Some(previous) = previous {
            assert!(
                previous < file.path.as_str(),
                "frontend generation files are not canonical"
            );
        }
        previous = Some(&file.path);
        assert!(
            file.sha256.len() == 64
                && file
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid frontend file digest"
        );

        let input = source.join(&file.path);
        reject_symlink_path(&source, &file.path);
        let bytes = fs::read(&input).expect("read manifest-reachable frontend file");
        assert_eq!(
            bytes.len() as u64,
            file.bytes,
            "frontend file byte count drift"
        );
        let digest = format!("{:x}", Sha256::digest(&bytes));
        assert_eq!(digest, file.sha256, "frontend file digest drift");

        let output = destination.join(&file.path);
        fs::create_dir_all(output.parent().expect("frontend output parent"))
            .expect("create frontend output parent");
        fs::write(output, bytes).expect("stage embedded frontend file");
        println!("cargo:rerun-if-changed={}", input.display());
    }
    let metrics = derive_metrics(&manifest, &public_authority, manifest_bytes.len() as u64);
    for ((name, actual), (_, maximum)) in metric_values(&metrics)
        .into_iter()
        .zip(metric_values(&budget_authority.maximum_bytes))
    {
        assert!(actual <= maximum, "frontend {name} budget exceeded");
    }
    let identity = GenerationIdentity {
        schema_version: manifest.schema_version,
        document_entry: &manifest.document_entry,
        script_entry: &manifest.script_entry,
        files: &manifest.files,
        graph: &manifest.graph,
    };
    assert_eq!(
        format!("{:x}", Sha256::digest(canonical_json(&identity))),
        manifest.generation_id
    );
    fs::write(destination.join("generation.json"), manifest_bytes)
        .expect("stage frontend generation manifest");
}

fn canonical_json<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = serde_json::to_string_pretty(value)
        .expect("serialize canonical frontend JSON")
        .into_bytes();
    bytes.push(b'\n');
    bytes
}

fn read_canonical_json<T: DeserializeOwned + Serialize>(path: &Path, label: &str) -> (Vec<u8>, T) {
    let metadata =
        fs::symlink_metadata(path).unwrap_or_else(|error| panic!("read {label}: {error}"));
    assert!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a real file"
    );
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {label}: {error}"));
    let value: T =
        serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("parse {label}: {error}"));
    assert_eq!(
        bytes,
        canonical_json(&value),
        "{label} must be canonical JSON"
    );
    (bytes, value)
}

fn total_file_bytes(files: &[&GenerationFile]) -> u64 {
    files.iter().fold(0_u64, |total, file| {
        total
            .checked_add(file.bytes)
            .expect("frontend file byte total overflow")
    })
}

fn largest_file(files: &[&GenerationFile]) -> u64 {
    files.iter().map(|file| file.bytes).max().unwrap_or(0)
}

fn derive_metrics(
    manifest: &GenerationManifest,
    public_authority: &PublicAssetAuthority,
    receipt_bytes: u64,
) -> BundleMetrics {
    let files_by_path = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<HashMap<_, _>>();
    assert_eq!(files_by_path.len(), manifest.files.len());

    let mut public_previous: Option<&str> = None;
    let mut public_portable = HashSet::new();
    for file_path in &public_authority.files {
        validate_relative_path(file_path);
        if let Some(previous) = public_previous {
            assert!(
                previous < file_path,
                "public asset authority is not canonical"
            );
        }
        public_previous = Some(file_path);
        assert!(
            public_portable.insert(file_path.to_ascii_lowercase()),
            "public asset authority contains a portable collision"
        );
    }
    let public_paths = public_authority
        .files
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let mut graph_previous: Option<&str> = None;
    let mut graph_by_path = HashMap::new();
    for output in &manifest.graph {
        validate_relative_path(&output.path);
        let extension = file_extension(&output.path);
        assert!(
            matches!(extension, "js" | "css"),
            "invalid graph output extension"
        );
        if let Some(previous) = graph_previous {
            assert!(
                previous < output.path.as_str(),
                "frontend graph is not canonical"
            );
        }
        graph_previous = Some(&output.path);
        assert!(
            graph_by_path.insert(output.path.as_str(), output).is_none(),
            "duplicate frontend graph output"
        );
        let mut import_previous: Option<(&str, bool)> = None;
        let mut imported_paths = HashSet::new();
        for imported in &output.imports {
            validate_relative_path(&imported.path);
            let order = (imported.path.as_str(), imported.dynamic);
            if let Some(previous) = import_previous {
                assert!(previous < order, "frontend graph imports are not canonical");
            }
            import_previous = Some(order);
            assert!(
                imported_paths.insert(imported.path.as_str()),
                "duplicate frontend graph import"
            );
        }
    }

    for file_path in files_by_path.keys() {
        let public = public_paths.contains(file_path);
        let generated = graph_by_path.contains_key(file_path);
        assert!(public ^ generated, "frontend file authority drift");
    }
    assert_eq!(
        public_paths.len() + graph_by_path.len(),
        files_by_path.len(),
        "frontend file authority drift"
    );
    assert!(public_paths.contains(manifest.document_entry.as_str()));
    assert!(graph_by_path.contains_key(manifest.script_entry.as_str()));

    for output in &manifest.graph {
        let extension = file_extension(&output.path);
        if let Some(css_bundle) = &output.css_bundle {
            validate_relative_path(css_bundle);
            assert_eq!(extension, "js", "only JavaScript may own a CSS bundle");
            assert_eq!(
                file_extension(css_bundle),
                "css",
                "invalid CSS bundle extension"
            );
            assert!(graph_by_path.contains_key(css_bundle.as_str()));
        }
        for imported in &output.imports {
            assert_eq!(
                file_extension(&imported.path),
                extension,
                "frontend graph edge extension drift"
            );
            assert!(graph_by_path.contains_key(imported.path.as_str()));
            assert!(!imported.dynamic || extension == "js");
        }
    }

    let initial_javascript =
        traverse_initial(vec![manifest.script_entry.clone()], "js", &graph_by_path);
    let css_seeds = initial_javascript
        .iter()
        .filter_map(|file_path| graph_by_path[file_path.as_str()].css_bundle.clone())
        .collect::<Vec<_>>();
    let initial_css = traverse_initial(css_seeds, "css", &graph_by_path);
    let initial = initial_javascript
        .iter()
        .chain(&initial_css)
        .cloned()
        .collect::<HashSet<_>>();
    let public_files = public_authority
        .files
        .iter()
        .map(|file_path| files_by_path[file_path.as_str()])
        .collect::<Vec<_>>();
    let generated_files = manifest
        .graph
        .iter()
        .map(|output| files_by_path[output.path.as_str()])
        .collect::<Vec<_>>();
    let files_for = |paths: &HashSet<String>| {
        paths
            .iter()
            .map(|file_path| files_by_path[file_path.as_str()])
            .collect::<Vec<_>>()
    };
    let lazy_files = manifest
        .graph
        .iter()
        .filter(|output| !initial.contains(&output.path))
        .map(|output| files_by_path[output.path.as_str()])
        .collect::<Vec<_>>();
    let public_assets = total_file_bytes(&public_files);
    let generated_total = total_file_bytes(&generated_files);
    BundleMetrics {
        initial_javascript: total_file_bytes(&files_for(&initial_javascript)),
        initial_css: total_file_bytes(&files_for(&initial_css)),
        lazy_total: total_file_bytes(&lazy_files),
        public_assets,
        largest_public_asset: largest_file(&public_files),
        largest_generated_output: largest_file(&generated_files),
        generated_total,
        packaged_payload: public_assets
            .checked_add(generated_total)
            .and_then(|total| total.checked_add(receipt_bytes))
            .expect("frontend packaged payload overflow"),
    }
}

fn traverse_initial(
    seeds: Vec<String>,
    extension: &str,
    graph: &HashMap<&str, &GenerationGraphOutput>,
) -> HashSet<String> {
    let mut reached = HashSet::new();
    let mut pending = seeds;
    while let Some(current) = pending.pop() {
        if reached.contains(&current) {
            continue;
        }
        assert_eq!(file_extension(&current), extension);
        let output = graph
            .get(current.as_str())
            .expect("initial frontend graph target");
        reached.insert(current);
        pending.extend(
            output
                .imports
                .iter()
                .filter(|imported| !imported.dynamic)
                .map(|imported| imported.path.clone()),
        );
    }
    reached
}

fn file_extension(value: &str) -> &str {
    Path::new(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
}

fn metric_values(metrics: &BundleMetrics) -> [(&'static str, u64); 8] {
    [
        ("initial_javascript", metrics.initial_javascript),
        ("initial_css", metrics.initial_css),
        ("lazy_total", metrics.lazy_total),
        ("public_assets", metrics.public_assets),
        ("largest_public_asset", metrics.largest_public_asset),
        ("largest_generated_output", metrics.largest_generated_output),
        ("generated_total", metrics.generated_total),
        ("packaged_payload", metrics.packaged_payload),
    ]
}

fn validate_relative_path(value: &str) {
    assert!(
        is_valid_frontend_relative_path(value),
        "invalid frontend relative path: {value}"
    );
}

fn reject_symlink_path(root: &Path, relative: &str) {
    let mut current = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            unreachable!("relative path was validated");
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current).expect("inspect frontend file path");
        assert!(
            !metadata.file_type().is_symlink(),
            "frontend file path contains a symlink"
        );
    }
}
