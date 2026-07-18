use crate::MANAGED_ARTIFACT_MAX_BYTES;
use crate::types::{
    CompositionPlan, CompositionState, CompositionTier, ManagedDependencyStateEdge,
    PerformanceMode, VersionFamily,
};
use reqwest::Url;
use sha2::{Digest, Sha512};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

const MAX_MANAGED_PLAN_NODES: usize = 256;
const MAX_MANAGED_PLAN_EDGES: usize = 4096;
const MAX_COMPOSITION_ID_BYTES: usize = 256;
const MAX_TARGET_VALUE_BYTES: usize = 128;
const MAX_FILENAME_BYTES: usize = 255;
const MAX_DOWNLOAD_URL_BYTES: usize = 8192;
const MODRINTH_ID_BYTES: usize = 8;
const SHA512_HEX_BYTES: usize = 128;

impl ManagedArtifactRole {
    fn canonical_name(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::RequiredDependency => "required_dependency",
        }
    }
}

pub use crate::types::ManagedArtifactRole;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedArtifactPin {
    project_id: String,
    version_id: String,
    filename: String,
    download_url: String,
    size: u64,
    sha512: String,
    role: ManagedArtifactRole,
}

impl ManagedArtifactPin {
    pub fn new(
        project_id: impl Into<String>,
        version_id: impl Into<String>,
        filename: impl Into<String>,
        download_url: impl Into<String>,
        size: u64,
        sha512: impl Into<String>,
        role: ManagedArtifactRole,
    ) -> Result<Self, ManagedInstallPlanError> {
        let pin = Self {
            project_id: project_id.into(),
            version_id: version_id.into(),
            filename: filename.into(),
            download_url: download_url.into(),
            size,
            sha512: sha512.into(),
            role,
        };
        pin.validate()?;
        Ok(pin)
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn version_id(&self) -> &str {
        &self.version_id
    }

    pub fn filename(&self) -> &str {
        &self.filename
    }

    pub fn download_url(&self) -> &str {
        &self.download_url
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sha512(&self) -> &str {
        &self.sha512
    }

    pub fn role(&self) -> ManagedArtifactRole {
        self.role
    }

    fn validate(&self) -> Result<(), ManagedInstallPlanError> {
        validate_modrinth_id(&self.project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&self.version_id)
            .map_err(|_| ManagedInstallPlanError::InvalidVersionId)?;
        validate_portable_jar_filename(&self.filename)?;
        validate_download_url(&self.download_url)?;
        if self.size == 0 || self.size > MANAGED_ARTIFACT_MAX_BYTES {
            return Err(ManagedInstallPlanError::InvalidArtifactSize);
        }
        validate_sha512(&self.sha512)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManagedDependencyEdge {
    parent_project_id: String,
    child_project_id: String,
    child_version_id: String,
}

impl ManagedDependencyEdge {
    pub fn new(
        parent_project_id: impl Into<String>,
        child_project_id: impl Into<String>,
        child_version_id: impl Into<String>,
    ) -> Result<Self, ManagedInstallPlanError> {
        let edge = Self {
            parent_project_id: parent_project_id.into(),
            child_project_id: child_project_id.into(),
            child_version_id: child_version_id.into(),
        };
        edge.validate()?;
        Ok(edge)
    }

    pub fn parent_project_id(&self) -> &str {
        &self.parent_project_id
    }

    pub fn child_project_id(&self) -> &str {
        &self.child_project_id
    }

    pub fn child_version_id(&self) -> &str {
        &self.child_version_id
    }

    fn validate(&self) -> Result<(), ManagedInstallPlanError> {
        validate_modrinth_id(&self.parent_project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&self.child_project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&self.child_version_id)
            .map_err(|_| ManagedInstallPlanError::InvalidVersionId)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCompositionInstallPlan {
    composition_id: String,
    family: VersionFamily,
    tier: CompositionTier,
    game_version: String,
    loader: String,
    pins: Vec<ManagedArtifactPin>,
    edges: Vec<ManagedDependencyEdge>,
    aggregate_bytes: u64,
    graph_digest: String,
}

impl ManagedCompositionInstallPlan {
    pub fn seal(
        declarative: CompositionPlan,
        game_version: impl Into<String>,
        loader: impl Into<String>,
        mut pins: Vec<ManagedArtifactPin>,
        mut edges: Vec<ManagedDependencyEdge>,
    ) -> Result<Self, ManagedInstallPlanError> {
        if declarative.mode != PerformanceMode::Managed {
            return Err(ManagedInstallPlanError::ManagedModeRequired);
        }
        validate_required_text(
            &declarative.composition_id,
            MAX_COMPOSITION_ID_BYTES,
            ManagedInstallPlanError::InvalidCompositionId,
        )?;

        let game_version = game_version.into();
        let loader = loader.into();
        validate_target_value(&game_version)?;
        validate_target_value(&loader)?;
        if loader != declarative.loader {
            return Err(ManagedInstallPlanError::LoaderMismatch);
        }
        if pins.len() > MAX_MANAGED_PLAN_NODES {
            return Err(ManagedInstallPlanError::TooManyArtifacts);
        }
        if edges.len() > MAX_MANAGED_PLAN_EDGES {
            return Err(ManagedInstallPlanError::TooManyEdges);
        }
        if declarative.mods.len() > MAX_MANAGED_PLAN_NODES {
            return Err(ManagedInstallPlanError::TooManyRoots);
        }

        let mut roots = BTreeSet::new();
        for managed_mod in &declarative.mods {
            validate_modrinth_id(&managed_mod.project_id)
                .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
            if !roots.insert(managed_mod.project_id.clone()) {
                return Err(ManagedInstallPlanError::DuplicateRoot);
            }
        }

        let mut pin_indices = BTreeMap::new();
        let mut filenames = BTreeSet::new();
        let mut aggregate_bytes = 0_u64;
        for (index, pin) in pins.iter().enumerate() {
            pin.validate()?;
            if pin_indices.insert(pin.project_id.clone(), index).is_some() {
                return Err(ManagedInstallPlanError::DuplicateArtifact);
            }
            if !filenames.insert(pin.filename.to_ascii_lowercase()) {
                return Err(ManagedInstallPlanError::DuplicateFilename);
            }
            aggregate_bytes = aggregate_bytes
                .checked_add(pin.size)
                .filter(|bytes| *bytes <= MANAGED_ARTIFACT_MAX_BYTES)
                .ok_or(ManagedInstallPlanError::ArtifactBytesExceeded)?;
        }

        let pin_roots = pins
            .iter()
            .filter(|pin| pin.role == ManagedArtifactRole::Root)
            .map(|pin| pin.project_id.clone())
            .collect::<BTreeSet<_>>();
        if roots != pin_roots {
            return Err(ManagedInstallPlanError::RootSetMismatch);
        }
        if roots.is_empty() && (!pins.is_empty() || !edges.is_empty()) {
            return Err(ManagedInstallPlanError::UnexpectedEmptyRootGraph);
        }

        let mut unique_edges = BTreeSet::new();
        let mut adjacency = BTreeMap::<String, Vec<String>>::new();
        for edge in &edges {
            edge.validate()?;
            if !unique_edges.insert(edge.clone()) {
                return Err(ManagedInstallPlanError::DuplicateEdge);
            }
            let Some(&parent_index) = pin_indices.get(&edge.parent_project_id) else {
                return Err(ManagedInstallPlanError::UnknownEdgeEndpoint);
            };
            let Some(&child_index) = pin_indices.get(&edge.child_project_id) else {
                return Err(ManagedInstallPlanError::UnknownEdgeEndpoint);
            };
            if pins[child_index].version_id != edge.child_version_id {
                return Err(ManagedInstallPlanError::DependencyVersionMismatch);
            }
            adjacency
                .entry(pins[parent_index].project_id.clone())
                .or_default()
                .push(pins[child_index].project_id.clone());
        }

        let mut reachable = roots.clone();
        let mut queue = roots.iter().cloned().collect::<VecDeque<_>>();
        while let Some(project_id) = queue.pop_front() {
            if let Some(children) = adjacency.get(&project_id) {
                for child in children {
                    if reachable.insert(child.clone()) {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
        if pins.iter().any(|pin| {
            pin.role == ManagedArtifactRole::RequiredDependency
                && !reachable.contains(&pin.project_id)
        }) {
            return Err(ManagedInstallPlanError::UnreachableDependency);
        }

        pins.sort_by(|left, right| left.project_id.cmp(&right.project_id));
        edges.sort();
        let graph_digest = graph_digest(
            &declarative.composition_id,
            declarative.family,
            declarative.tier,
            &game_version,
            &loader,
            aggregate_bytes,
            pins.iter().map(CanonicalGraphNode::from_pin),
            edges.iter().map(CanonicalGraphEdge::from_plan),
        );

        Ok(Self {
            composition_id: declarative.composition_id,
            family: declarative.family,
            tier: declarative.tier,
            game_version,
            loader,
            pins,
            edges,
            aggregate_bytes,
            graph_digest,
        })
    }

    pub fn composition_id(&self) -> &str {
        &self.composition_id
    }

    pub fn family(&self) -> VersionFamily {
        self.family
    }

    pub fn tier(&self) -> CompositionTier {
        self.tier
    }

    pub fn game_version(&self) -> &str {
        &self.game_version
    }

    pub fn loader(&self) -> &str {
        &self.loader
    }

    pub fn pins(&self) -> &[ManagedArtifactPin] {
        &self.pins
    }

    pub fn edges(&self) -> &[ManagedDependencyEdge] {
        &self.edges
    }

    pub fn aggregate_bytes(&self) -> u64 {
        self.aggregate_bytes
    }

    pub fn graph_digest(&self) -> &str {
        &self.graph_digest
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManagedInstallPlanError {
    #[error("managed composition mode is required")]
    ManagedModeRequired,
    #[error("managed composition id is invalid")]
    InvalidCompositionId,
    #[error("managed composition target is invalid")]
    InvalidTarget,
    #[error("managed composition loader does not match its exact target")]
    LoaderMismatch,
    #[error("managed artifact project id is invalid")]
    InvalidProjectId,
    #[error("managed artifact version id is invalid")]
    InvalidVersionId,
    #[error("managed artifact filename is not a portable jar basename")]
    InvalidFilename,
    #[error("managed artifact download url is invalid")]
    InvalidDownloadUrl,
    #[error("managed artifact size is invalid")]
    InvalidArtifactSize,
    #[error("managed artifact SHA-512 is not canonical")]
    InvalidSha512,
    #[error("managed composition has too many declarative roots")]
    TooManyRoots,
    #[error("managed composition has too many artifacts")]
    TooManyArtifacts,
    #[error("managed composition has too many dependency edges")]
    TooManyEdges,
    #[error("managed composition artifact bytes exceed the staging bound")]
    ArtifactBytesExceeded,
    #[error("managed composition declares a duplicate root")]
    DuplicateRoot,
    #[error("managed composition contains a duplicate artifact")]
    DuplicateArtifact,
    #[error("managed composition contains a non-portable filename collision")]
    DuplicateFilename,
    #[error("managed composition root artifacts do not match its declarative roots")]
    RootSetMismatch,
    #[error("managed composition without roots must have an empty graph")]
    UnexpectedEmptyRootGraph,
    #[error("managed composition contains a duplicate dependency edge")]
    DuplicateEdge,
    #[error("managed composition dependency edge has an unknown endpoint")]
    UnknownEdgeEndpoint,
    #[error("managed composition dependency edge does not pin the exact child version")]
    DependencyVersionMismatch,
    #[error("managed composition contains an unreachable required dependency")]
    UnreachableDependency,
    #[error("managed composition graph digest is invalid")]
    InvalidGraphDigest,
}

fn validate_modrinth_id(value: &str) -> Result<(), ()> {
    if value.len() == MODRINTH_ID_BYTES && value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(())
    }
}

fn validate_required_text(
    value: &str,
    max_bytes: usize,
    error: ManagedInstallPlanError,
) -> Result<(), ManagedInstallPlanError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(error)
    } else {
        Ok(())
    }
}

fn validate_target_value(value: &str) -> Result<(), ManagedInstallPlanError> {
    validate_required_text(
        value,
        MAX_TARGET_VALUE_BYTES,
        ManagedInstallPlanError::InvalidTarget,
    )?;
    if value.is_ascii() {
        Ok(())
    } else {
        Err(ManagedInstallPlanError::InvalidTarget)
    }
}

fn validate_portable_jar_filename(filename: &str) -> Result<(), ManagedInstallPlanError> {
    let invalid_ascii = |byte: u8| {
        byte.is_ascii_control()
            || matches!(
                byte,
                b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
            )
    };
    if filename.is_empty()
        || filename.len() > MAX_FILENAME_BYTES
        || !filename.is_ascii()
        || filename.trim() != filename
        || !filename.to_ascii_lowercase().ends_with(".jar")
        || filename.bytes().any(invalid_ascii)
    {
        return Err(ManagedInstallPlanError::InvalidFilename);
    }

    let stem = filename
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if stem.is_empty() {
        return Err(ManagedInstallPlanError::InvalidFilename);
    }
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if reserved {
        Err(ManagedInstallPlanError::InvalidFilename)
    } else {
        Ok(())
    }
}

fn validate_download_url(url: &str) -> Result<(), ManagedInstallPlanError> {
    if url.is_empty()
        || url.len() > MAX_DOWNLOAD_URL_BYTES
        || url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(ManagedInstallPlanError::InvalidDownloadUrl);
    }
    let parsed = Url::parse(url).map_err(|_| ManagedInstallPlanError::InvalidDownloadUrl)?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        Err(ManagedInstallPlanError::InvalidDownloadUrl)
    } else {
        Ok(())
    }
}

fn validate_sha512(sha512: &str) -> Result<(), ManagedInstallPlanError> {
    if sha512.len() == SHA512_HEX_BYTES
        && sha512
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(ManagedInstallPlanError::InvalidSha512)
    }
}

#[derive(Clone, Copy)]
struct CanonicalGraphNode<'a> {
    project_id: &'a str,
    version_id: &'a str,
    filename: &'a str,
    role: ManagedArtifactRole,
    size: u64,
    sha512: &'a str,
}

impl<'a> CanonicalGraphNode<'a> {
    fn from_pin(pin: &'a ManagedArtifactPin) -> Self {
        Self {
            project_id: &pin.project_id,
            version_id: &pin.version_id,
            filename: &pin.filename,
            role: pin.role,
            size: pin.size,
            sha512: &pin.sha512,
        }
    }

    fn from_installed(installed: &'a crate::types::InstalledMod) -> Self {
        Self {
            project_id: &installed.project_id,
            version_id: &installed.version_id,
            filename: &installed.filename,
            role: installed.role,
            size: installed.size,
            sha512: &installed.integrity.sha512,
        }
    }
}

#[derive(Clone, Copy)]
struct CanonicalGraphEdge<'a> {
    parent_project_id: &'a str,
    child_project_id: &'a str,
    child_version_id: &'a str,
}

impl<'a> CanonicalGraphEdge<'a> {
    fn from_plan(edge: &'a ManagedDependencyEdge) -> Self {
        Self {
            parent_project_id: &edge.parent_project_id,
            child_project_id: &edge.child_project_id,
            child_version_id: &edge.child_version_id,
        }
    }

    fn from_state(edge: &'a ManagedDependencyStateEdge) -> Self {
        Self {
            parent_project_id: &edge.parent_project_id,
            child_project_id: &edge.child_project_id,
            child_version_id: &edge.child_version_id,
        }
    }
}

fn graph_digest<'a>(
    composition_id: &str,
    family: VersionFamily,
    tier: CompositionTier,
    game_version: &str,
    loader: &str,
    aggregate_bytes: u64,
    pins: impl ExactSizeIterator<Item = CanonicalGraphNode<'a>>,
    edges: impl ExactSizeIterator<Item = CanonicalGraphEdge<'a>>,
) -> String {
    let mut hasher = Sha512::new();
    hash_field(
        &mut hasher,
        b"domain",
        b"axial.managed-composition-install-plan.v1",
    );
    hash_field(&mut hasher, b"composition", composition_id.as_bytes());
    hash_field(&mut hasher, b"family", family_name(family).as_bytes());
    hash_field(&mut hasher, b"tier", tier_name(tier).as_bytes());
    hash_field(&mut hasher, b"game_version", game_version.as_bytes());
    hash_field(&mut hasher, b"loader", loader.as_bytes());
    hash_field(
        &mut hasher,
        b"aggregate_bytes",
        &aggregate_bytes.to_be_bytes(),
    );
    let pin_count = pins.len();
    hash_field(&mut hasher, b"pin_count", &(pin_count as u64).to_be_bytes());
    for pin in pins {
        hash_field(&mut hasher, b"pin_project", pin.project_id.as_bytes());
        hash_field(&mut hasher, b"pin_version", pin.version_id.as_bytes());
        hash_field(&mut hasher, b"pin_filename", pin.filename.as_bytes());
        hash_field(
            &mut hasher,
            b"pin_role",
            pin.role.canonical_name().as_bytes(),
        );
        hash_field(&mut hasher, b"pin_size", &pin.size.to_be_bytes());
        hash_field(&mut hasher, b"pin_sha512", pin.sha512.as_bytes());
    }
    let edge_count = edges.len();
    hash_field(
        &mut hasher,
        b"edge_count",
        &(edge_count as u64).to_be_bytes(),
    );
    for edge in edges {
        hash_field(
            &mut hasher,
            b"edge_parent",
            edge.parent_project_id.as_bytes(),
        );
        hash_field(&mut hasher, b"edge_child", edge.child_project_id.as_bytes());
        hash_field(
            &mut hasher,
            b"edge_child_version",
            edge.child_version_id.as_bytes(),
        );
    }
    hex::encode(hasher.finalize())
}

pub(crate) fn validate_state_graph(
    state: &CompositionState,
) -> Result<(), ManagedInstallPlanError> {
    let digest = canonical_state_graph_digest(state)?;
    if digest != state.graph_sha512 {
        return Err(ManagedInstallPlanError::InvalidGraphDigest);
    }
    Ok(())
}

pub(crate) fn canonical_state_graph_digest(
    state: &CompositionState,
) -> Result<String, ManagedInstallPlanError> {
    if state.installed_mods.len() > MAX_MANAGED_PLAN_NODES {
        return Err(ManagedInstallPlanError::TooManyArtifacts);
    }
    if state.dependency_edges.len() > MAX_MANAGED_PLAN_EDGES {
        return Err(ManagedInstallPlanError::TooManyEdges);
    }
    validate_required_text(
        &state.composition_id,
        MAX_COMPOSITION_ID_BYTES,
        ManagedInstallPlanError::InvalidCompositionId,
    )?;
    validate_target_value(&state.game_version)?;
    validate_target_value(&state.loader)?;

    let mut projects = BTreeMap::new();
    let mut filenames = BTreeSet::new();
    let mut aggregate_bytes = 0_u64;
    let mut roots = BTreeSet::new();
    let mut previous_project = None;
    for (index, installed) in state.installed_mods.iter().enumerate() {
        validate_modrinth_id(&installed.project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&installed.version_id)
            .map_err(|_| ManagedInstallPlanError::InvalidVersionId)?;
        validate_portable_jar_filename(&installed.filename)?;
        if installed.size == 0 || installed.size > MANAGED_ARTIFACT_MAX_BYTES {
            return Err(ManagedInstallPlanError::InvalidArtifactSize);
        }
        validate_sha512(&installed.integrity.sha512)?;
        if previous_project.is_some_and(|previous: &str| previous >= installed.project_id.as_str())
        {
            return Err(ManagedInstallPlanError::DuplicateArtifact);
        }
        previous_project = Some(&installed.project_id);
        if projects
            .insert(installed.project_id.clone(), index)
            .is_some()
        {
            return Err(ManagedInstallPlanError::DuplicateArtifact);
        }
        if !filenames.insert(installed.filename.to_ascii_lowercase()) {
            return Err(ManagedInstallPlanError::DuplicateFilename);
        }
        if installed.role == ManagedArtifactRole::Root {
            roots.insert(installed.project_id.clone());
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(installed.size)
            .filter(|bytes| *bytes <= MANAGED_ARTIFACT_MAX_BYTES)
            .ok_or(ManagedInstallPlanError::ArtifactBytesExceeded)?;
    }

    let mut unique_edges = BTreeSet::new();
    let mut adjacency = BTreeMap::<String, Vec<String>>::new();
    let mut previous_edge = None;
    for edge in &state.dependency_edges {
        if previous_edge.is_some_and(|previous: &ManagedDependencyStateEdge| previous >= edge) {
            return Err(ManagedInstallPlanError::DuplicateEdge);
        }
        previous_edge = Some(edge);
        validate_modrinth_id(&edge.parent_project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&edge.child_project_id)
            .map_err(|_| ManagedInstallPlanError::InvalidProjectId)?;
        validate_modrinth_id(&edge.child_version_id)
            .map_err(|_| ManagedInstallPlanError::InvalidVersionId)?;
        if !unique_edges.insert(edge.clone()) {
            return Err(ManagedInstallPlanError::DuplicateEdge);
        }
        let Some(&child_index) = projects.get(&edge.child_project_id) else {
            return Err(ManagedInstallPlanError::UnknownEdgeEndpoint);
        };
        if !projects.contains_key(&edge.parent_project_id) {
            return Err(ManagedInstallPlanError::UnknownEdgeEndpoint);
        }
        if state.installed_mods[child_index].version_id != edge.child_version_id {
            return Err(ManagedInstallPlanError::DependencyVersionMismatch);
        }
        adjacency
            .entry(edge.parent_project_id.clone())
            .or_default()
            .push(edge.child_project_id.clone());
    }

    let mut reachable = roots.clone();
    let mut queue = roots.into_iter().collect::<VecDeque<_>>();
    while let Some(project_id) = queue.pop_front() {
        if let Some(children) = adjacency.get(&project_id) {
            for child in children {
                if reachable.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
    }
    if state.installed_mods.iter().any(|installed| {
        installed.role == ManagedArtifactRole::RequiredDependency
            && !reachable.contains(&installed.project_id)
    }) {
        return Err(ManagedInstallPlanError::UnreachableDependency);
    }

    let digest = graph_digest(
        &state.composition_id,
        state.family,
        state.tier,
        &state.game_version,
        &state.loader,
        aggregate_bytes,
        state
            .installed_mods
            .iter()
            .map(CanonicalGraphNode::from_installed),
        state
            .dependency_edges
            .iter()
            .map(CanonicalGraphEdge::from_state),
    );
    Ok(digest)
}

fn hash_field(hasher: &mut Sha512, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn family_name(family: VersionFamily) -> &'static str {
    match family {
        VersionFamily::A => "A",
        VersionFamily::B => "B",
        VersionFamily::C => "C",
        VersionFamily::D => "D",
        VersionFamily::E => "E",
        VersionFamily::F => "F",
    }
}

fn tier_name(tier: CompositionTier) -> &'static str {
    match tier {
        CompositionTier::Extended => "extended",
        CompositionTier::Core => "core",
        CompositionTier::VanillaEnhanced => "vanilla_enhanced",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ManagedMod, ModCondition};

    const ROOT_A: &str = "AANobbMI";
    const ROOT_B: &str = "gvQqBUqZ";
    const DEP_A: &str = "P7dR8mSH";
    const DEP_B: &str = "9s6osm5g";
    const VERSION_A: &str = "NFkjnzWE";
    const VERSION_B: &str = "1234abcd";
    const SHA512: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn managed_mod(project_id: &str) -> ManagedMod {
        ManagedMod {
            artifact_id: format!("artifact-{project_id}"),
            project_id: project_id.to_string(),
            slug: String::new(),
            name: project_id.to_string(),
            condition: ModCondition::Always,
            version_range: String::new(),
            exact_game_versions: Vec::new(),
            hardware_req: None,
            mutual_exclusions: Vec::new(),
        }
    }

    fn declarative(roots: &[&str]) -> CompositionPlan {
        CompositionPlan {
            composition_id: "family-f-fabric-extended".to_string(),
            family: VersionFamily::F,
            loader: "fabric".to_string(),
            mode: PerformanceMode::Managed,
            tier: CompositionTier::Extended,
            mods: roots.iter().map(|root| managed_mod(root)).collect(),
            jvm_preset: String::new(),
            warnings: Vec::new(),
            fallback_reason: String::new(),
        }
    }

    fn pin(project_id: &str, version_id: &str, role: ManagedArtifactRole) -> ManagedArtifactPin {
        ManagedArtifactPin::new(
            project_id,
            version_id,
            format!("{project_id}.jar"),
            format!("https://cdn.modrinth.com/data/{project_id}/{version_id}.jar"),
            64,
            SHA512,
            role,
        )
        .expect("valid pin")
    }

    fn edge(parent: &str, child: &str, child_version: &str) -> ManagedDependencyEdge {
        ManagedDependencyEdge::new(parent, child, child_version).expect("valid edge")
    }

    fn seal(
        roots: &[&str],
        pins: Vec<ManagedArtifactPin>,
        edges: Vec<ManagedDependencyEdge>,
    ) -> Result<ManagedCompositionInstallPlan, ManagedInstallPlanError> {
        ManagedCompositionInstallPlan::seal(declarative(roots), "1.21.11", "fabric", pins, edges)
    }

    #[test]
    fn seals_dependency_complete_graph_and_exposes_exact_facts() {
        let sealed = seal(
            &[ROOT_A, ROOT_B],
            vec![
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
                pin(ROOT_B, VERSION_B, ManagedArtifactRole::Root),
                pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
            ],
            vec![
                edge(ROOT_B, DEP_A, VERSION_B),
                edge(ROOT_A, DEP_A, VERSION_B),
            ],
        )
        .expect("sealed graph");

        assert_eq!(sealed.composition_id(), "family-f-fabric-extended");
        assert_eq!(sealed.family(), VersionFamily::F);
        assert_eq!(sealed.tier(), CompositionTier::Extended);
        assert_eq!(sealed.game_version(), "1.21.11");
        assert_eq!(sealed.loader(), "fabric");
        assert_eq!(sealed.aggregate_bytes(), 192);
        assert_eq!(sealed.graph_digest().len(), SHA512_HEX_BYTES);
        let dependency = sealed
            .pins()
            .iter()
            .find(|pin| pin.project_id() == DEP_A)
            .expect("dependency pin");
        assert_eq!(dependency.version_id(), VERSION_B);
        assert_eq!(dependency.filename(), "P7dR8mSH.jar");
        assert_eq!(dependency.size(), 64);
        assert_eq!(dependency.sha512(), SHA512);
        assert_eq!(dependency.role(), ManagedArtifactRole::RequiredDependency);
        assert!(dependency.download_url().starts_with("https://"));
        assert_eq!(sealed.edges()[0].parent_project_id(), ROOT_A);
        assert_eq!(sealed.edges()[0].child_project_id(), DEP_A);
        assert_eq!(sealed.edges()[0].child_version_id(), VERSION_B);
    }

    #[test]
    fn canonical_digest_is_input_order_independent_and_excludes_url() {
        let first = seal(
            &[ROOT_A, ROOT_B],
            vec![
                pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                pin(ROOT_B, VERSION_B, ManagedArtifactRole::Root),
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
            ],
            vec![
                edge(ROOT_A, DEP_A, VERSION_B),
                edge(ROOT_B, DEP_A, VERSION_B),
            ],
        )
        .expect("first graph");
        let mut changed_url = pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root);
        changed_url.download_url = "https://mirror.example.test/root-a.jar?token=other".to_string();
        let second = seal(
            &[ROOT_A, ROOT_B],
            vec![
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
                pin(ROOT_B, VERSION_B, ManagedArtifactRole::Root),
                changed_url,
            ],
            vec![
                edge(ROOT_B, DEP_A, VERSION_B),
                edge(ROOT_A, DEP_A, VERSION_B),
            ],
        )
        .expect("second graph");

        assert_eq!(first.graph_digest(), second.graph_digest());
        assert_eq!(
            first
                .pins()
                .iter()
                .map(ManagedArtifactPin::project_id)
                .collect::<Vec<_>>(),
            second
                .pins()
                .iter()
                .map(ManagedArtifactPin::project_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(first.edges(), second.edges());
    }

    #[test]
    fn canonical_digest_binds_target_composition_artifacts_and_edges() {
        let baseline = seal(
            &[ROOT_A],
            vec![
                pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
            ],
            vec![edge(ROOT_A, DEP_A, VERSION_B)],
        )
        .expect("baseline");
        let mut changed_composition = declarative(&[ROOT_A]);
        changed_composition.composition_id = "other-composition".to_string();
        let changed_composition = ManagedCompositionInstallPlan::seal(
            changed_composition,
            "1.21.11",
            "fabric",
            baseline.pins.clone(),
            baseline.edges.clone(),
        )
        .expect("changed composition");
        let changed_target = ManagedCompositionInstallPlan::seal(
            declarative(&[ROOT_A]),
            "1.21.10",
            "fabric",
            baseline.pins.clone(),
            baseline.edges.clone(),
        )
        .expect("changed target");
        let mut changed_artifact = baseline.pins.clone();
        changed_artifact[1].sha512 = "f".repeat(SHA512_HEX_BYTES);
        let changed_artifact =
            seal(&[ROOT_A], changed_artifact, baseline.edges.clone()).expect("changed artifact");
        let with_root_cycle = seal(
            &[ROOT_A],
            baseline.pins.clone(),
            vec![
                edge(ROOT_A, DEP_A, VERSION_B),
                edge(DEP_A, ROOT_A, VERSION_A),
            ],
        )
        .expect("changed edges");

        for changed in [
            changed_composition,
            changed_target,
            changed_artifact,
            with_root_cycle,
        ] {
            assert_ne!(baseline.graph_digest(), changed.graph_digest());
        }
    }

    #[test]
    fn empty_declarative_composition_seals_only_an_empty_graph() {
        assert!(seal(&[], Vec::new(), Vec::new()).is_ok());
        assert_eq!(
            seal(
                &[],
                vec![pin(
                    DEP_A,
                    VERSION_A,
                    ManagedArtifactRole::RequiredDependency
                )],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::UnexpectedEmptyRootGraph)
        );
    }

    #[test]
    fn rejects_non_managed_or_mismatched_targets() {
        let root = pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root);
        let mut plan = declarative(&[ROOT_A]);
        plan.mode = PerformanceMode::Custom;
        assert_eq!(
            ManagedCompositionInstallPlan::seal(
                plan,
                "1.21.11",
                "fabric",
                vec![root.clone()],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::ManagedModeRequired)
        );
        assert_eq!(
            ManagedCompositionInstallPlan::seal(
                declarative(&[ROOT_A]),
                "1.21.11",
                "forge",
                vec![root.clone()],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::LoaderMismatch)
        );
        assert_eq!(
            ManagedCompositionInstallPlan::seal(
                declarative(&[ROOT_A]),
                " 1.21.11",
                "fabric",
                vec![root],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::InvalidTarget)
        );
    }

    #[test]
    fn pin_constructor_rejects_malformed_provider_facts() {
        let valid = || {
            (
                ROOT_A.to_string(),
                VERSION_A.to_string(),
                "sodium.jar".to_string(),
                "https://cdn.modrinth.com/sodium.jar".to_string(),
                1_u64,
                SHA512.to_string(),
            )
        };
        let cases = [
            {
                let mut value = valid();
                value.0 = "sodium".to_string();
                (value, ManagedInstallPlanError::InvalidProjectId)
            },
            {
                let mut value = valid();
                value.1 = "version-too-long".to_string();
                (value, ManagedInstallPlanError::InvalidVersionId)
            },
            {
                let mut value = valid();
                value.2 = "../sodium.jar".to_string();
                (value, ManagedInstallPlanError::InvalidFilename)
            },
            {
                let mut value = valid();
                value.2 = "CON.jar".to_string();
                (value, ManagedInstallPlanError::InvalidFilename)
            },
            {
                let mut value = valid();
                value.3 = "http://cdn.modrinth.com/sodium.jar".to_string();
                (value, ManagedInstallPlanError::InvalidDownloadUrl)
            },
            {
                let mut value = valid();
                value.3 = "https://user@example.test/sodium.jar".to_string();
                (value, ManagedInstallPlanError::InvalidDownloadUrl)
            },
            {
                let mut value = valid();
                value.3 = "https://cdn.modrinth.com/sodium jar".to_string();
                (value, ManagedInstallPlanError::InvalidDownloadUrl)
            },
            {
                let mut value = valid();
                value.3 = "https://cdn.modrinth.com/sodium.jar\n".to_string();
                (value, ManagedInstallPlanError::InvalidDownloadUrl)
            },
            {
                let mut value = valid();
                value.4 = 0;
                (value, ManagedInstallPlanError::InvalidArtifactSize)
            },
            {
                let mut value = valid();
                value.4 = MANAGED_ARTIFACT_MAX_BYTES + 1;
                (value, ManagedInstallPlanError::InvalidArtifactSize)
            },
            {
                let mut value = valid();
                value.5 = "A".repeat(SHA512_HEX_BYTES);
                (value, ManagedInstallPlanError::InvalidSha512)
            },
        ];

        for ((project, version, filename, url, size, sha512), expected) in cases {
            assert_eq!(
                ManagedArtifactPin::new(
                    project,
                    version,
                    filename,
                    url,
                    size,
                    sha512,
                    ManagedArtifactRole::Root,
                ),
                Err(expected)
            );
        }
    }

    #[test]
    fn seal_defensively_revalidates_private_pin_and_edge_facts() {
        let mut invalid_pin = pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root);
        invalid_pin.sha512 = "not-a-digest".to_string();
        assert_eq!(
            seal(&[ROOT_A], vec![invalid_pin], Vec::new()),
            Err(ManagedInstallPlanError::InvalidSha512)
        );

        let invalid_edge = ManagedDependencyEdge {
            parent_project_id: ROOT_A.to_string(),
            child_project_id: DEP_A.to_string(),
            child_version_id: "slug".to_string(),
        };
        assert_eq!(
            seal(
                &[ROOT_A],
                vec![
                    pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                    pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
                ],
                vec![invalid_edge],
            ),
            Err(ManagedInstallPlanError::InvalidVersionId)
        );
    }

    #[test]
    fn rejects_root_identity_and_portable_filename_ambiguity() {
        let mut duplicate_roots = declarative(&[ROOT_A, ROOT_A]);
        assert_eq!(
            ManagedCompositionInstallPlan::seal(
                duplicate_roots.clone(),
                "1.21.11",
                "fabric",
                vec![pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root)],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::DuplicateRoot)
        );
        duplicate_roots.mods[1].project_id = "not-slug".to_string();
        assert_eq!(
            ManagedCompositionInstallPlan::seal(
                duplicate_roots,
                "1.21.11",
                "fabric",
                vec![pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root)],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::InvalidProjectId)
        );

        assert_eq!(
            seal(
                &[ROOT_A, ROOT_B],
                vec![
                    pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                    pin(ROOT_A, VERSION_B, ManagedArtifactRole::Root),
                ],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::DuplicateArtifact)
        );
        let first = pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root);
        let mut second = pin(ROOT_B, VERSION_B, ManagedArtifactRole::Root);
        second.filename = first.filename.to_ascii_lowercase();
        assert_eq!(
            seal(&[ROOT_A, ROOT_B], vec![first, second], Vec::new()),
            Err(ManagedInstallPlanError::DuplicateFilename)
        );
    }

    #[test]
    fn requires_root_set_equality() {
        assert_eq!(
            seal(
                &[ROOT_A],
                vec![pin(
                    ROOT_A,
                    VERSION_A,
                    ManagedArtifactRole::RequiredDependency
                )],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::RootSetMismatch)
        );
        assert_eq!(
            seal(
                &[ROOT_A],
                vec![pin(ROOT_B, VERSION_A, ManagedArtifactRole::Root)],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::RootSetMismatch)
        );
    }

    #[test]
    fn rejects_bad_edges_and_unreachable_dependencies() {
        let pins = || {
            vec![
                pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
            ]
        };
        assert_eq!(
            seal(&[ROOT_A], pins(), vec![edge(ROOT_A, DEP_B, VERSION_B)]),
            Err(ManagedInstallPlanError::UnknownEdgeEndpoint)
        );
        assert_eq!(
            seal(&[ROOT_A], pins(), vec![edge(ROOT_A, DEP_A, VERSION_A)]),
            Err(ManagedInstallPlanError::DependencyVersionMismatch)
        );
        assert_eq!(
            seal(&[ROOT_A], pins(), Vec::new()),
            Err(ManagedInstallPlanError::UnreachableDependency)
        );
        let duplicate = edge(ROOT_A, DEP_A, VERSION_B);
        assert_eq!(
            seal(&[ROOT_A], pins(), vec![duplicate.clone(), duplicate]),
            Err(ManagedInstallPlanError::DuplicateEdge)
        );
    }

    #[test]
    fn accepts_required_cycles_and_a_root_as_a_child() {
        let sealed = seal(
            &[ROOT_A],
            vec![
                pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root),
                pin(DEP_A, VERSION_B, ManagedArtifactRole::RequiredDependency),
            ],
            vec![
                edge(ROOT_A, DEP_A, VERSION_B),
                edge(DEP_A, ROOT_A, VERSION_A),
                edge(DEP_A, DEP_A, VERSION_B),
            ],
        )
        .expect("root-reachable exact cycle");
        assert_eq!(sealed.edges().len(), 3);
    }

    #[test]
    fn enforces_node_edge_and_aggregate_byte_bounds() {
        let oversized_roots = vec![ROOT_A; MAX_MANAGED_PLAN_NODES + 1];
        assert_eq!(
            seal(&oversized_roots, Vec::new(), Vec::new()),
            Err(ManagedInstallPlanError::TooManyRoots)
        );

        let root = pin(ROOT_A, VERSION_A, ManagedArtifactRole::Root);
        assert_eq!(
            seal(
                &[ROOT_A],
                vec![root.clone(); MAX_MANAGED_PLAN_NODES + 1],
                Vec::new(),
            ),
            Err(ManagedInstallPlanError::TooManyArtifacts)
        );
        assert_eq!(
            seal(
                &[ROOT_A],
                vec![root.clone()],
                vec![edge(ROOT_A, ROOT_A, VERSION_A); MAX_MANAGED_PLAN_EDGES + 1],
            ),
            Err(ManagedInstallPlanError::TooManyEdges)
        );

        let mut first = root;
        first.size = MANAGED_ARTIFACT_MAX_BYTES;
        let mut second = pin(ROOT_B, VERSION_B, ManagedArtifactRole::Root);
        second.size = 1;
        assert_eq!(
            seal(&[ROOT_A, ROOT_B], vec![first, second], Vec::new()),
            Err(ManagedInstallPlanError::ArtifactBytesExceeded)
        );
    }
}
