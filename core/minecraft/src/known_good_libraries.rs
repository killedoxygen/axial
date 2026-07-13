use crate::artifact_path::ArtifactRelativePath;
use crate::download::{
    DownloadJob, ExactLibraryDownloadProof, LibraryArtifactPlan,
    MaterializedSelectedArtifactSource, library_artifact_plans_for,
};
use crate::launch::{VersionJson, effective_java_version_for};
use crate::rules::Environment;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedLibraryKind {
    Library,
    Native,
}

struct SealedExactLibraryDeclaration {
    path: ArtifactRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
}

pub(crate) struct SealedExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    version_contract: VersionJson,
}

pub(crate) struct PendingExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<ArtifactRelativePath, LibraryArtifactPlan>,
    version_contract: VersionJson,
}

pub(crate) struct PendingStreamedLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
    selected: BTreeMap<ArtifactRelativePath, LibraryArtifactPlan>,
    version_contract: VersionJson,
}

pub(crate) struct AuthenticatedVanillaLibraryDeclarationSource {
    declarations: PendingExactLibraryDeclarations,
    bytes: Arc<[u8]>,
    size: u64,
    sha1: [u8; 20],
}

impl AuthenticatedVanillaLibraryDeclarationSource {
    pub(crate) fn into_parts(self) -> (PendingExactLibraryDeclarations, Arc<[u8]>, u64, [u8; 20]) {
        (self.declarations, self.bytes, self.size, self.sha1)
    }
}

pub(crate) struct ClassifiedLibraryDownload {
    pub(crate) job: DownloadJob,
    pub(crate) acquisition: LibraryAcquisition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryAcquisition {
    ExactDeclaration,
    FreshStream,
}

struct StreamedExactLibraryProof {
    path: ArtifactRelativePath,
    kind: SealedLibraryKind,
    sha1: [u8; 20],
    size: u64,
    provider_url: String,
    expected: crate::download::ExpectedIntegrity,
}

impl StreamedExactLibraryProof {
    fn from_authenticated_stream(
        path: ArtifactRelativePath,
        kind: SealedLibraryKind,
        provider_url: String,
        expected: crate::download::ExpectedIntegrity,
        sha1: [u8; 20],
        size: u64,
    ) -> Result<Self, SealedLibraryDeclarationError> {
        if size == 0 {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        Ok(Self {
            path,
            kind,
            sha1,
            size,
            provider_url,
            expected,
        })
    }
}

impl SealedExactLibraryDeclarations {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn get(
        &self,
        path: &ArtifactRelativePath,
    ) -> Option<(SealedLibraryKind, [u8; 20], u64)> {
        self.entries
            .get(path)
            .map(|entry| (entry.kind, entry.sha1, entry.size))
    }

    pub(crate) fn matches_version(&self, version: &VersionJson) -> bool {
        self.version_contract == *version
    }
}

impl PendingExactLibraryDeclarations {
    pub(crate) fn classify_jobs(
        self,
        libraries_root: &Path,
        jobs: Vec<DownloadJob>,
    ) -> Result<
        (
            PendingStreamedLibraryDeclarations,
            Vec<ClassifiedLibraryDownload>,
        ),
        SealedLibraryDeclarationError,
    > {
        let mut jobs = jobs.into_iter().try_fold(
            BTreeMap::new(),
            |mut jobs, job| -> Result<_, SealedLibraryDeclarationError> {
                if jobs.insert(job.relative_path.clone(), job).is_some() {
                    return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
                }
                Ok(jobs)
            },
        )?;
        if jobs.len() != self.selected.len() {
            return Err(SealedLibraryDeclarationError::MissingDeclaration);
        }
        let mut classified = Vec::with_capacity(self.selected.len());
        for (path, plan) in &self.selected {
            let job = jobs
                .remove(path)
                .ok_or(SealedLibraryDeclarationError::MissingDeclaration)?;
            if job.is_native != plan.is_native {
                return Err(SealedLibraryDeclarationError::KindDrift);
            }
            if plan.source_url.as_deref() != Some(job.url.as_str())
                || job.expected != plan.expected
                || job.path != path.join_under(libraries_root)
            {
                return Err(SealedLibraryDeclarationError::ContractDrift);
            }
            classified.push(ClassifiedLibraryDownload {
                acquisition: if self.entries.contains_key(path) {
                    LibraryAcquisition::ExactDeclaration
                } else {
                    LibraryAcquisition::FreshStream
                },
                job,
            });
        }
        if !jobs.is_empty() {
            return Err(SealedLibraryDeclarationError::ExtraDeclaration);
        }
        Ok((
            PendingStreamedLibraryDeclarations {
                entries: self.entries,
                selected: self.selected,
                version_contract: self.version_contract,
            },
            classified,
        ))
    }
}

impl PendingStreamedLibraryDeclarations {
    pub(crate) fn seal_streamed(
        self,
        streamed: Vec<ExactLibraryDownloadProof>,
    ) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
        let streamed = streamed
            .into_iter()
            .map(|proof| {
                let (path, is_native, provider_url, expected, size, sha1) = proof.into_parts();
                StreamedExactLibraryProof::from_authenticated_stream(
                    path,
                    if is_native {
                        SealedLibraryKind::Native
                    } else {
                        SealedLibraryKind::Library
                    },
                    provider_url,
                    expected,
                    sha1,
                    size,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        merge_selected_library_declarations(
            self.selected.into_values().collect(),
            AuthenticatedExactLibraryDeclarations {
                entries: self.entries,
            },
            streamed,
            self.version_contract,
        )
    }
}

pub(crate) fn seal_vanilla_exact_library_declarations(
    source: MaterializedSelectedArtifactSource,
    resolved: &VersionJson,
    environment: &Environment,
) -> Result<AuthenticatedVanillaLibraryDeclarationSource, SealedLibraryDeclarationError> {
    let mut authenticated = serde_json::from_slice::<VersionJson>(source.bytes())
        .map_err(|_| SealedLibraryDeclarationError::AncestorMismatch)?;
    if authenticated.asset_index.id.is_empty() && !authenticated.assets.is_empty() {
        authenticated
            .asset_index
            .id
            .clone_from(&authenticated.assets);
    }
    authenticated.java_version = effective_java_version_for(
        &authenticated.id,
        &authenticated.kind,
        &authenticated.java_version,
    );
    if authenticated != *resolved {
        return Err(SealedLibraryDeclarationError::AncestorMismatch);
    }
    let plans = library_artifact_plans_for(&resolved.libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let declarations = seal_exact_plan_subset(plans, authenticated)?;
    let (bytes, size, sha1) = source.into_parts();
    Ok(AuthenticatedVanillaLibraryDeclarationSource {
        declarations,
        bytes,
        size,
        sha1,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SealedLibraryDeclarationError {
    AncestorMismatch,
    InvalidSelectedPlan,
    InvalidExactDeclaration,
    MissingDeclaration,
    ExtraDeclaration,
    DuplicateDeclaration,
    KindDrift,
    ContractDrift,
    MissingStreamSource,
}

struct AuthenticatedExactLibraryDeclarations {
    entries: BTreeMap<ArtifactRelativePath, SealedExactLibraryDeclaration>,
}

impl AuthenticatedExactLibraryDeclarations {
    fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    fn insert(
        &mut self,
        entry: SealedExactLibraryDeclaration,
    ) -> Result<(), SealedLibraryDeclarationError> {
        if entry.size == 0 {
            return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
        }
        if self.entries.insert(entry.path.clone(), entry).is_some() {
            return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
        }
        Ok(())
    }
}

fn merge_selected_library_declarations(
    plans: Vec<LibraryArtifactPlan>,
    mut exact: AuthenticatedExactLibraryDeclarations,
    streamed: Vec<StreamedExactLibraryProof>,
    version_contract: VersionJson,
) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let mut selected = BTreeMap::new();
    for plan in plans {
        if selected.insert(plan.relative_path.clone(), plan).is_some() {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
    }
    let mut streamed = streamed
        .into_iter()
        .map(|proof| (proof.path.clone(), proof))
        .try_fold(BTreeMap::new(), |mut proofs, (path, proof)| {
            if proofs.insert(path, proof).is_some() {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            Ok(proofs)
        })?;
    let mut complete = BTreeMap::new();
    for (path, plan) in selected {
        let selected_kind = kind_for_plan(&plan);
        let entry = match (exact.entries.remove(&path), streamed.remove(&path)) {
            (Some(_), Some(_)) => {
                return Err(SealedLibraryDeclarationError::DuplicateDeclaration);
            }
            (Some(entry), None) => entry,
            (None, Some(proof)) => {
                if plan.source_url.as_ref() != Some(&proof.provider_url)
                    || plan.expected != proof.expected
                {
                    return Err(SealedLibraryDeclarationError::MissingStreamSource);
                }
                SealedExactLibraryDeclaration {
                    path: proof.path,
                    kind: proof.kind,
                    sha1: proof.sha1,
                    size: proof.size,
                }
            }
            (None, None) => return Err(SealedLibraryDeclarationError::MissingDeclaration),
        };
        if entry.path != path || entry.kind != selected_kind {
            return Err(SealedLibraryDeclarationError::KindDrift);
        }
        validate_plan_contract(&plan, entry.sha1, entry.size)?;
        complete.insert(path, entry);
    }
    if !exact.entries.is_empty() || !streamed.is_empty() {
        return Err(SealedLibraryDeclarationError::ExtraDeclaration);
    }
    Ok(SealedExactLibraryDeclarations {
        entries: complete,
        version_contract,
    })
}

fn seal_exact_plan_subset(
    plans: Vec<LibraryArtifactPlan>,
    version_contract: VersionJson,
) -> Result<PendingExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let mut selected = BTreeMap::new();
    let mut exact = AuthenticatedExactLibraryDeclarations::empty();
    for plan in plans {
        if plan.expected.size.is_some_and(|size| size > 0)
            && plan.expected.sha1.as_deref().is_some()
        {
            let size = plan.expected.size.expect("checked exact size");
            let sha1 = decode_sha1(plan.expected.sha1.as_deref().expect("checked exact digest"))?;
            exact.insert(SealedExactLibraryDeclaration {
                path: plan.relative_path.clone(),
                kind: kind_for_plan(&plan),
                sha1,
                size,
            })?;
        }
        if selected.insert(plan.relative_path.clone(), plan).is_some() {
            return Err(SealedLibraryDeclarationError::InvalidSelectedPlan);
        }
    }
    Ok(PendingExactLibraryDeclarations {
        entries: exact.entries,
        selected,
        version_contract,
    })
}

fn decode_sha1(value: &str) -> Result<[u8; 20], SealedLibraryDeclarationError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SealedLibraryDeclarationError::InvalidExactDeclaration);
    }
    let mut digest = [0_u8; 20];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or(SealedLibraryDeclarationError::InvalidExactDeclaration)?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or(SealedLibraryDeclarationError::InvalidExactDeclaration)?;
        digest[index] = ((high << 4) | low) as u8;
    }
    Ok(digest)
}

#[cfg(test)]
pub(crate) fn seal_vanilla_library_declarations_for_test(
    version: &VersionJson,
    environment: &Environment,
    streamed: Vec<ExactLibraryDownloadProof>,
) -> Result<SealedExactLibraryDeclarations, SealedLibraryDeclarationError> {
    let plans = library_artifact_plans_for(&version.libraries, environment)
        .map_err(|_| SealedLibraryDeclarationError::InvalidSelectedPlan)?;
    let pending = seal_exact_plan_subset(plans, version.clone())?;
    PendingStreamedLibraryDeclarations {
        entries: pending.entries,
        selected: pending.selected,
        version_contract: pending.version_contract,
    }
    .seal_streamed(streamed)
}

fn kind_for_plan(plan: &LibraryArtifactPlan) -> SealedLibraryKind {
    if plan.is_native {
        SealedLibraryKind::Native
    } else {
        SealedLibraryKind::Library
    }
}

fn validate_plan_contract(
    plan: &LibraryArtifactPlan,
    sha1: [u8; 20],
    size: u64,
) -> Result<(), SealedLibraryDeclarationError> {
    if size == 0 || plan.expected.size.is_some_and(|expected| expected != size) {
        return Err(SealedLibraryDeclarationError::ContractDrift);
    }
    if let Some(expected) = plan.expected.sha1.as_deref() {
        let mut observed = String::with_capacity(40);
        use std::fmt::Write as _;
        for byte in sha1 {
            let _ = write!(&mut observed, "{byte:02x}");
        }
        if !expected.eq_ignore_ascii_case(&observed) {
            return Err(SealedLibraryDeclarationError::ContractDrift);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::ExpectedIntegrity;

    fn plan(path: &str, exact: bool, source: bool, native: bool) -> LibraryArtifactPlan {
        LibraryArtifactPlan {
            relative_path: ArtifactRelativePath::new(path).unwrap(),
            source_url: source.then(|| "https://example.invalid/library.jar".to_string()),
            name: path.to_string(),
            expected: if exact {
                ExpectedIntegrity {
                    size: Some(7),
                    sha1: Some("0101010101010101010101010101010101010101".to_string()),
                }
            } else {
                ExpectedIntegrity::default()
            },
            is_native: native,
        }
    }

    fn exact(path: &str, native: bool) -> SealedExactLibraryDeclaration {
        SealedExactLibraryDeclaration {
            path: ArtifactRelativePath::new(path).unwrap(),
            kind: if native {
                SealedLibraryKind::Native
            } else {
                SealedLibraryKind::Library
            },
            sha1: [1; 20],
            size: 7,
        }
    }

    fn stream(path: &str, native: bool) -> StreamedExactLibraryProof {
        StreamedExactLibraryProof::from_authenticated_stream(
            ArtifactRelativePath::new(path).unwrap(),
            if native {
                SealedLibraryKind::Native
            } else {
                SealedLibraryKind::Library
            },
            "https://example.invalid/library.jar".to_string(),
            ExpectedIntegrity::default(),
            [1; 20],
            7,
        )
        .unwrap()
    }

    fn version_contract() -> VersionJson {
        serde_json::from_str(r#"{"id":"fixture"}"#).expect("fixture version contract")
    }

    #[test]
    fn consumes_exact_and_streamed_subsets_one_to_one() {
        let mut declarations = AuthenticatedExactLibraryDeclarations::empty();
        declarations.insert(exact("exact.jar", false)).unwrap();

        let sealed = merge_selected_library_declarations(
            vec![
                plan("exact.jar", true, false, false),
                plan("stream.jar", false, true, true),
            ],
            declarations,
            vec![stream("stream.jar", true)],
            version_contract(),
        )
        .unwrap();

        assert_eq!(sealed.len(), 2);
    }

    #[test]
    fn exact_declaration_does_not_require_artifact_url() {
        let mut declarations = AuthenticatedExactLibraryDeclarations::empty();
        declarations.insert(exact("exact.jar", false)).unwrap();

        assert!(
            merge_selected_library_declarations(
                vec![plan("exact.jar", true, false, false)],
                declarations,
                Vec::new(),
                version_contract(),
            )
            .is_ok()
        );
    }

    #[test]
    fn incomplete_declaration_requires_artifact_url() {
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("stream.jar", false, false, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("stream.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::MissingStreamSource)
        ));
    }

    #[test]
    fn rejects_missing_extra_duplicate_kind_and_contract_drift() {
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("missing.jar", false, true, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                Vec::new(),
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::MissingDeclaration)
        ));
        assert!(matches!(
            merge_selected_library_declarations(
                Vec::new(),
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("extra.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::ExtraDeclaration)
        ));
        let mut exact_and_stream = AuthenticatedExactLibraryDeclarations::empty();
        exact_and_stream
            .insert(exact("duplicate.jar", false))
            .unwrap();
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("duplicate.jar", true, true, false)],
                exact_and_stream,
                vec![stream("duplicate.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::DuplicateDeclaration)
        ));
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("native.jar", false, true, true)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![stream("native.jar", false)],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::KindDrift)
        ));
        let drift = StreamedExactLibraryProof::from_authenticated_stream(
            ArtifactRelativePath::new("drift.jar").unwrap(),
            SealedLibraryKind::Library,
            "https://example.invalid/library.jar".to_string(),
            ExpectedIntegrity {
                size: Some(7),
                sha1: Some("0101010101010101010101010101010101010101".to_string()),
            },
            [2; 20],
            7,
        )
        .unwrap();
        assert!(matches!(
            merge_selected_library_declarations(
                vec![plan("drift.jar", true, true, false)],
                AuthenticatedExactLibraryDeclarations::empty(),
                vec![drift],
                version_contract(),
            ),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));
    }

    #[test]
    fn preclassification_rejects_exact_job_url_expected_and_destination_drift() {
        let root = std::path::PathBuf::from("/managed/libraries");
        let selected = plan("exact.jar", true, true, false);
        let job = || DownloadJob {
            relative_path: selected.relative_path.clone(),
            path: selected.relative_path.join_under(&root),
            url: selected.source_url.clone().unwrap(),
            name: selected.name.clone(),
            expected: selected.expected.clone(),
            is_native: false,
        };
        let pending =
            || seal_exact_plan_subset(vec![selected.clone()], version_contract()).unwrap();

        let (_, classified) = pending().classify_jobs(&root, vec![job()]).unwrap();
        assert_eq!(classified.len(), 1);
        assert_eq!(
            classified[0].acquisition,
            LibraryAcquisition::ExactDeclaration
        );

        let mut url_drift = job();
        url_drift.url = "https://other.invalid/library.jar".to_string();
        assert!(matches!(
            pending().classify_jobs(&root, vec![url_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let mut expected_drift = job();
        expected_drift.expected.size = Some(8);
        assert!(matches!(
            pending().classify_jobs(&root, vec![expected_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));

        let mut destination_drift = job();
        destination_drift.path = root.join("other.jar");
        assert!(matches!(
            pending().classify_jobs(&root, vec![destination_drift]),
            Err(SealedLibraryDeclarationError::ContractDrift)
        ));
    }
}
