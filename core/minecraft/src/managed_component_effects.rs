use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::MAX_TIER2_ARTIFACT_BYTES;
use crate::loaders::types::LoaderError;
use crate::managed_component_publication::{
    COMPONENT_INTENT_FILE, COMPONENT_OUTCOME_FILE, COMPONENT_QUARANTINE_DIRECTORY,
    COMPONENT_SETTLEMENT_FILE, COMPONENT_STAGING_DIRECTORY, COMPONENT_TABLE_DIRECTORY,
    component_lane_name,
};
use crate::managed_component_spool::{ComponentTableReplay, ComponentTableSpoolError};
use crate::managed_component_table::{
    ComponentIntentManifest, ComponentTableError, ComponentTableParser, ComponentTableSummary,
    MAX_COMPONENT_TABLE_SHARD_BYTES, MAX_COMPONENT_TABLE_SHARDS, ManagedComponentKind,
    component_table_path, decode_component_table_shard,
};
use crate::managed_fs::{ManagedDir, ManagedFileGuard};
use crate::managed_publication::{ManagedPublicationError, ManagedRootPublicationLease};
use std::collections::BTreeSet;

const MAX_COMPONENT_LANE_ENTRIES: usize = 6;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ComponentEffectsError {
    #[error("managed component filesystem topology is invalid")]
    Topology,
    #[error(transparent)]
    Filesystem(#[from] LoaderError),
    #[error(transparent)]
    Publication(#[from] ManagedPublicationError),
    #[error(transparent)]
    Table(#[from] ComponentTableError),
    #[error(transparent)]
    Spool(#[from] ComponentTableSpoolError),
}

pub(crate) struct ComponentLane {
    component: ManagedComponentKind,
    lane: ManagedDir,
    table: ManagedDir,
    staging: ManagedDir,
    quarantine: ManagedDir,
}

pub(crate) struct ComponentDurableTable {
    summary: ComponentTableSummary,
    shard_guards: Vec<ManagedFileGuard>,
}

pub(crate) struct ComponentCanonicalPathPlan {
    creation_anchor: ManagedDir,
    remaining_parent_segments: Vec<String>,
    file_name: String,
    first_created_depth: Option<u16>,
}

pub(crate) enum ComponentCanonicalObservation {
    Absent,
    Regular(ComponentObservedFile),
}

pub(crate) struct ComponentObservedFile {
    parent: ManagedDir,
    file_name: String,
    guard: ManagedFileGuard,
    size: u64,
    sha1: [u8; 20],
}

impl ComponentLane {
    pub(crate) fn prepare_fresh(
        lease: &ManagedRootPublicationLease,
        component: ManagedComponentKind,
    ) -> Result<Self, ComponentEffectsError> {
        lease.revalidate()?;
        let publication = lease.publication_directory();
        let lane_name = component_lane_name(component);
        let lane = open_or_create_exact_child(publication, lane_name)?;
        let names = exact_entry_names(&lane, MAX_COMPONENT_LANE_ENTRIES + 1)?;
        if names
            .iter()
            .any(|name| !component_lane_entry_is_known(name))
            || names.contains(COMPONENT_INTENT_FILE)
            || names.contains(COMPONENT_OUTCOME_FILE)
            || names.contains(COMPONENT_SETTLEMENT_FILE)
        {
            return Err(ComponentEffectsError::Topology);
        }

        let table = open_or_create_exact_child(&lane, COMPONENT_TABLE_DIRECTORY)?;
        let staging = open_or_create_exact_child(&lane, COMPONENT_STAGING_DIRECTORY)?;
        let quarantine = open_or_create_exact_child(&lane, COMPONENT_QUARANTINE_DIRECTORY)?;
        if !exact_entry_names(&staging, 1)?.is_empty()
            || !exact_entry_names(&quarantine, 1)?.is_empty()
        {
            return Err(ComponentEffectsError::Topology);
        }
        cleanup_preintent_table(&table, component)?;
        lane.sync()?;
        publication.sync()?;
        lease.root().sync()?;

        Ok(Self {
            component,
            lane,
            table,
            staging,
            quarantine,
        })
    }

    pub(crate) fn component(&self) -> ManagedComponentKind {
        self.component
    }

    pub(crate) fn lane(&self) -> &ManagedDir {
        &self.lane
    }

    pub(crate) fn staging(&self) -> &ManagedDir {
        &self.staging
    }

    pub(crate) fn quarantine(&self) -> &ManagedDir {
        &self.quarantine
    }

    pub(crate) fn publish_table(
        &self,
        mut replay: ComponentTableReplay,
        manifest: &ComponentIntentManifest,
    ) -> Result<ComponentDurableTable, ComponentEffectsError> {
        if manifest.component != self.component || !exact_entry_names(&self.table, 1)?.is_empty() {
            return Err(ComponentEffectsError::Topology);
        }
        let _validated_manifest = ComponentTableParser::new(manifest.clone())?;
        let mut shard_guards = Vec::new();
        shard_guards
            .try_reserve_exact(manifest.shards.len())
            .map_err(|_| ComponentEffectsError::Topology)?;
        let mut next_shard = 0_usize;
        while let Some((descriptor, encoded)) = replay.next()? {
            let expected = manifest
                .shards
                .get(next_shard)
                .ok_or(ComponentEffectsError::Topology)?;
            if &descriptor != expected {
                return Err(ComponentEffectsError::Topology);
            }
            let name = component_table_file_name(next_shard)?;
            let guard = self.table.write_new_exact_guarded(&name, &encoded)?;
            self.table.sync()?;
            if !self.table.file_guard_matches(&name, &guard)? {
                return Err(ComponentEffectsError::Topology);
            }
            shard_guards.push(guard);
            next_shard += 1;
        }
        if next_shard != manifest.shards.len() {
            return Err(ComponentEffectsError::Topology);
        }
        self.lane.sync()?;
        self.validate_table_guards(manifest, shard_guards)
    }

    pub(crate) fn read_table(
        &self,
        manifest: &ComponentIntentManifest,
    ) -> Result<ComponentDurableTable, ComponentEffectsError> {
        if manifest.component != self.component {
            return Err(ComponentEffectsError::Topology);
        }
        let expected_names = (0..manifest.shards.len())
            .map(component_table_file_name)
            .collect::<Result<BTreeSet<_>, _>>()?;
        if exact_entry_names(&self.table, manifest.shards.len().saturating_add(1))?
            != expected_names
        {
            return Err(ComponentEffectsError::Topology);
        }

        let mut shard_guards = Vec::new();
        shard_guards
            .try_reserve_exact(manifest.shards.len())
            .map_err(|_| ComponentEffectsError::Topology)?;
        for index in 0..manifest.shards.len() {
            let name = component_table_file_name(index)?;
            let guard = self
                .table
                .inspect_regular_file(&name)?
                .ok_or(ComponentEffectsError::Topology)?;
            shard_guards.push(guard);
        }
        self.validate_table_guards(manifest, shard_guards)
    }

    fn validate_table_guards(
        &self,
        manifest: &ComponentIntentManifest,
        shard_guards: Vec<ManagedFileGuard>,
    ) -> Result<ComponentDurableTable, ComponentEffectsError> {
        if shard_guards.len() != manifest.shards.len() {
            return Err(ComponentEffectsError::Topology);
        }
        let mut parser = ComponentTableParser::new(manifest.clone())?;
        for (index, (descriptor, guard)) in manifest.shards.iter().zip(&shard_guards).enumerate() {
            let name = component_table_file_name(index)?;
            if guard.size() != u64::from(descriptor.byte_len)
                || guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64
            {
                return Err(ComponentEffectsError::Topology);
            }
            let encoded = self.table.read_guarded_file_bounded(
                &name,
                &guard,
                MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
            )?;
            parser.parse_next(&encoded)?;
        }
        Ok(ComponentDurableTable {
            summary: parser.finish()?,
            shard_guards,
        })
    }
}

impl ComponentDurableTable {
    pub(crate) fn summary(&self) -> &ComponentTableSummary {
        &self.summary
    }

    pub(crate) fn shard_count(&self) -> usize {
        self.shard_guards.len()
    }
}

impl ComponentCanonicalPathPlan {
    pub(crate) fn first_created_depth(&self) -> Option<u16> {
        self.first_created_depth
    }

    pub(crate) fn creation_anchor(&self) -> &ManagedDir {
        &self.creation_anchor
    }

    pub(crate) fn remaining_parent_segments(&self) -> &[String] {
        &self.remaining_parent_segments
    }

    pub(crate) fn parent(&self) -> Option<&ManagedDir> {
        self.remaining_parent_segments
            .is_empty()
            .then_some(&self.creation_anchor)
    }

    pub(crate) fn file_name(&self) -> &str {
        &self.file_name
    }

    pub(crate) fn observe(&self) -> Result<ComponentCanonicalObservation, ComponentEffectsError> {
        if let Some(first_missing) = self.remaining_parent_segments.first() {
            if self
                .creation_anchor
                .has_portably_exact_child_name(first_missing)?
            {
                return Err(ComponentEffectsError::Topology);
            }
            return Ok(ComponentCanonicalObservation::Absent);
        }
        let parent = &self.creation_anchor;
        let _ = parent.has_portably_exact_child_name(&self.file_name)?;
        let Some(guard) = parent.inspect_regular_file(&self.file_name)? else {
            return Ok(ComponentCanonicalObservation::Absent);
        };
        let size = guard.size();
        if size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(ComponentEffectsError::Topology);
        }
        let sha1 =
            parent.sha1_guarded_file_bytes(&self.file_name, &guard, MAX_TIER2_ARTIFACT_BYTES)?;
        Ok(ComponentCanonicalObservation::Regular(
            ComponentObservedFile {
                parent: parent.clone(),
                file_name: self.file_name.clone(),
                guard,
                size,
                sha1,
            },
        ))
    }
}

impl ComponentObservedFile {
    pub(crate) fn parent(&self) -> &ManagedDir {
        &self.parent
    }

    pub(crate) fn file_name(&self) -> &str {
        &self.file_name
    }

    pub(crate) fn guard(&self) -> &ManagedFileGuard {
        &self.guard
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn sha1(&self) -> [u8; 20] {
        self.sha1
    }
}

pub(crate) fn plan_component_canonical_path(
    root: &ManagedDir,
    component: ManagedComponentKind,
    relative: &ArtifactRelativePath,
) -> Result<ComponentCanonicalPathPlan, ComponentEffectsError> {
    root.revalidate()?;
    let segment_count = relative.as_str().split('/').count();
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(segment_count)
        .map_err(|_| ComponentEffectsError::Topology)?;
    segments.extend(relative.as_str().split('/'));
    let file_name = copy_bounded_string(segments.pop().ok_or(ComponentEffectsError::Topology)?)?;
    let component_root_name = component_lane_name(component);
    if !root.has_portably_exact_child_name(component_root_name)? {
        let parent_count = segments
            .len()
            .checked_add(1)
            .ok_or(ComponentEffectsError::Topology)?;
        let mut remaining_parent_segments = Vec::new();
        remaining_parent_segments
            .try_reserve_exact(parent_count)
            .map_err(|_| ComponentEffectsError::Topology)?;
        remaining_parent_segments.push(copy_bounded_string(component_root_name)?);
        for segment in segments {
            remaining_parent_segments.push(copy_bounded_string(segment)?);
        }
        return Ok(ComponentCanonicalPathPlan {
            creation_anchor: root.clone(),
            remaining_parent_segments,
            file_name,
            first_created_depth: Some(0),
        });
    }

    let mut parent = root.open_child(component_root_name)?;
    for (index, segment) in segments.iter().copied().enumerate() {
        if !parent.has_portably_exact_child_name(segment)? {
            let mut remaining_parent_segments = Vec::new();
            remaining_parent_segments
                .try_reserve_exact(segments.len() - index)
                .map_err(|_| ComponentEffectsError::Topology)?;
            for missing in &segments[index..] {
                remaining_parent_segments.push(copy_bounded_string(missing)?);
            }
            return Ok(ComponentCanonicalPathPlan {
                creation_anchor: parent,
                remaining_parent_segments,
                file_name,
                first_created_depth: Some(
                    u16::try_from(index + 1).map_err(|_| ComponentEffectsError::Topology)?,
                ),
            });
        }
        parent = parent.open_child(segment)?;
    }
    // Reject a portable alias during planning; observation repeats the check.
    let _ = parent.has_portably_exact_child_name(&file_name)?;
    Ok(ComponentCanonicalPathPlan {
        creation_anchor: parent,
        remaining_parent_segments: Vec::new(),
        file_name,
        first_created_depth: None,
    })
}

fn copy_bounded_string(value: &str) -> Result<String, ComponentEffectsError> {
    let mut copied = String::new();
    copied
        .try_reserve_exact(value.len())
        .map_err(|_| ComponentEffectsError::Topology)?;
    copied.push_str(value);
    Ok(copied)
}

fn cleanup_preintent_table(
    table: &ManagedDir,
    component: ManagedComponentKind,
) -> Result<(), ComponentEffectsError> {
    let names = exact_entry_names(table, MAX_COMPONENT_TABLE_SHARDS + 1)?;
    let mut guarded = Vec::new();
    guarded
        .try_reserve_exact(names.len())
        .map_err(|_| ComponentEffectsError::Topology)?;
    let mut transaction_binding = None;
    for (index, name) in names.into_iter().enumerate() {
        if name != component_table_file_name(index)? {
            return Err(ComponentEffectsError::Topology);
        }
        let guard = table
            .inspect_regular_file(&name)?
            .ok_or(ComponentEffectsError::Topology)?;
        if guard.size() > MAX_COMPONENT_TABLE_SHARD_BYTES as u64 {
            return Err(ComponentEffectsError::Topology);
        }
        let encoded = table.read_guarded_file_bounded(
            &name,
            &guard,
            MAX_COMPONENT_TABLE_SHARD_BYTES as u64,
        )?;
        let shard = decode_component_table_shard(&encoded)?;
        let binding = (
            shard.shard_count,
            shard.total_rows,
            shard.transaction_nonce,
            shard.root_binding_sha256,
        );
        if shard.component != component
            || usize::try_from(shard.shard_index).map_err(|_| ComponentEffectsError::Topology)?
                != index
            || usize::try_from(shard.shard_count).map_err(|_| ComponentEffectsError::Topology)?
                < index + 1
            || transaction_binding.is_some_and(|expected| expected != binding)
        {
            return Err(ComponentEffectsError::Topology);
        }
        transaction_binding.get_or_insert(binding);
        guarded.push((name, guard));
    }

    // Durable reverse-prefix cleanup makes every crash point another valid prefix.
    for (name, guard) in guarded.iter().rev() {
        table.remove_guarded_file(name, guard)?;
        table.sync()?;
    }
    Ok(())
}

fn open_or_create_exact_child(
    parent: &ManagedDir,
    name: &str,
) -> Result<ManagedDir, ComponentEffectsError> {
    if parent.has_portably_exact_child_name(name)? {
        parent.open_child(name).map_err(Into::into)
    } else {
        parent.create_child_new(name).map_err(Into::into)
    }
}

fn exact_entry_names(
    directory: &ManagedDir,
    limit: usize,
) -> Result<BTreeSet<String>, ComponentEffectsError> {
    if limit == 0 {
        return Err(ComponentEffectsError::Topology);
    }
    let entries = directory.entries_bounded(limit)?;
    if entries.len() >= limit {
        return Err(ComponentEffectsError::Topology);
    }
    entries
        .into_iter()
        .map(|name| {
            name.into_string()
                .map_err(|_| ComponentEffectsError::Topology)
        })
        .collect()
}

fn component_lane_entry_is_known(name: &str) -> bool {
    matches!(
        name,
        COMPONENT_TABLE_DIRECTORY
            | COMPONENT_STAGING_DIRECTORY
            | COMPONENT_QUARANTINE_DIRECTORY
            | COMPONENT_INTENT_FILE
            | COMPONENT_OUTCOME_FILE
            | COMPONENT_SETTLEMENT_FILE
    )
}

fn component_table_file_name(index: usize) -> Result<String, ComponentEffectsError> {
    component_table_path(index)?
        .strip_prefix("table/")
        .map(str::to_string)
        .ok_or(ComponentEffectsError::Topology)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_component_spool::ComponentTableSpool;
    use crate::managed_component_table::{
        ComponentPriorFile, ComponentTableBuilder, ComponentTableRow, ManagedComponentArtifactKind,
    };
    use std::fs;

    #[tokio::test]
    async fn fresh_lane_has_only_the_closed_create_only_topology() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();

        assert_eq!(
            exact_entry_names(lane.lane(), MAX_COMPONENT_LANE_ENTRIES + 1).unwrap(),
            BTreeSet::from([
                COMPONENT_QUARANTINE_DIRECTORY.to_string(),
                COMPONENT_STAGING_DIRECTORY.to_string(),
                COMPONENT_TABLE_DIRECTORY.to_string(),
            ])
        );
        assert_eq!(lane.component(), ManagedComponentKind::Libraries);
        assert!(lane.staging().entries_bounded(1).unwrap().is_empty());
        assert!(lane.quarantine().entries_bounded(1).unwrap().is_empty());
        assert!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).is_ok(),
            "an exact empty fresh topology is reusable before intent"
        );
    }

    #[tokio::test]
    async fn fresh_lane_rejects_unknown_or_retained_preintent_entries() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        lane.lane().write_new_exact("unexpected", b"owned").unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Topology)
        ));

        lane.lane()
            .remove_guarded_file(
                "unexpected",
                &lane
                    .lane()
                    .inspect_regular_file("unexpected")
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        lane.staging().write_new_exact("000", b"owned").unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Topology)
        ));
    }

    #[test]
    fn canonical_walk_reports_exact_missing_depth_and_observes_a_stable_file() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/library.jar"),
            b"authenticated-library",
        )
        .unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let existing = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
        )
        .unwrap();
        assert_eq!(existing.first_created_depth(), None);
        assert_eq!(existing.file_name(), "library.jar");
        assert!(existing.parent().is_some());
        assert!(existing.remaining_parent_segments().is_empty());
        let ComponentCanonicalObservation::Regular(observed) = existing.observe().unwrap() else {
            panic!("existing regular file was not observed")
        };
        assert_eq!(observed.size(), 21);
        assert_ne!(observed.sha1(), [0; 20]);
        assert_eq!(observed.file_name(), "library.jar");
        assert!(
            observed
                .parent()
                .file_guard_matches(observed.file_name(), observed.guard())
                .unwrap()
        );

        let missing_parent = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/missing/library.jar").unwrap(),
        )
        .unwrap();
        assert_eq!(missing_parent.first_created_depth(), Some(2));
        assert!(missing_parent.parent().is_none());
        assert_eq!(
            missing_parent.remaining_parent_segments(),
            &["missing".to_string()]
        );
        let stable_org = root
            .open_child("libraries")
            .unwrap()
            .open_child("org")
            .unwrap();
        assert_eq!(
            missing_parent.creation_anchor().identity().unwrap(),
            stable_org.identity().unwrap()
        );
        assert!(matches!(
            missing_parent.observe().unwrap(),
            ComponentCanonicalObservation::Absent
        ));
        fs::create_dir(temporary.path().join("libraries/org/missing")).unwrap();
        assert!(
            missing_parent.observe().is_err(),
            "a created ancestor must invalidate the recorded missing depth"
        );

        let missing_root = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Assets,
            &ArtifactRelativePath::new("indexes/current.json").unwrap(),
        )
        .unwrap();
        assert_eq!(missing_root.first_created_depth(), Some(0));
        assert_eq!(
            missing_root.remaining_parent_segments(),
            &["assets".to_string(), "indexes".to_string()]
        );
        assert_eq!(
            missing_root.creation_anchor().identity().unwrap(),
            root.identity().unwrap()
        );
    }

    #[test]
    fn canonical_walk_rejects_portable_ancestor_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/Org/example")).unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        assert!(
            plan_component_canonical_path(
                &root,
                ManagedComponentKind::Libraries,
                &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_observation_rechecks_portable_leaf_aliases() {
        let temporary = tempfile::tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("libraries/org/example")).unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let plan = plan_component_canonical_path(
            &root,
            ManagedComponentKind::Libraries,
            &ArtifactRelativePath::new("org/example/library.jar").unwrap(),
        )
        .unwrap();
        fs::write(
            temporary.path().join("libraries/org/example/Library.jar"),
            b"portable-alias",
        )
        .unwrap();

        assert!(plan.observe().is_err());
    }

    #[tokio::test]
    async fn table_publication_replays_create_new_and_parses_the_durable_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = ManagedDir::open_root(temporary.path()).unwrap();
        let lease = ManagedRootPublicationLease::acquire(root).await.unwrap();
        let lane = ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        let digest = [0x55; 20];
        let mut builder =
            ComponentTableBuilder::new(ManagedComponentKind::Libraries, 1, [0x11; 16], [0x22; 32])
                .unwrap();
        let (encoded, descriptor) = builder
            .push_shard(vec![ComponentTableRow {
                inventory_ordinal: 0,
                final_size: 7,
                final_sha1: digest,
                kind: ManagedComponentArtifactKind::Library,
                path: ArtifactRelativePath::new("example/library.jar").unwrap(),
                first_created_depth: None,
                prior: Some(ComponentPriorFile {
                    size: 7,
                    sha1: digest,
                }),
            }])
            .unwrap();
        let (manifest, expected_summary) = builder.finish().unwrap();
        let mut invalid_manifest = manifest.clone();
        invalid_manifest.total_rows += 1;
        let mut invalid_spool = ComponentTableSpool::new(1).unwrap();
        invalid_spool
            .append(encoded.clone(), descriptor.clone())
            .unwrap();
        let invalid_replay = invalid_spool.finish(&manifest).unwrap();
        assert!(
            lane.publish_table(invalid_replay, &invalid_manifest)
                .is_err()
        );
        assert!(lane.table.entries_bounded(1).unwrap().is_empty());

        let mut spool = ComponentTableSpool::new(1).unwrap();
        spool.append(encoded, descriptor).unwrap();
        let replay = spool.finish(&manifest).unwrap();

        let durable = lane.publish_table(replay, &manifest).unwrap();
        assert_eq!(durable.summary(), &expected_summary);
        assert_eq!(durable.shard_count(), 1);
        let reopened = lane.read_table(&manifest).unwrap();
        assert_eq!(reopened.summary(), &expected_summary);
        assert_eq!(reopened.shard_count(), 1);
        drop((durable, reopened, lane));

        let cleaned =
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries).unwrap();
        assert!(cleaned.table.entries_bounded(1).unwrap().is_empty());
        cleaned
            .table
            .write_new_exact("000000.tbl", b"not-an-owned-table")
            .unwrap();
        assert!(matches!(
            ComponentLane::prepare_fresh(&lease, ManagedComponentKind::Libraries),
            Err(ComponentEffectsError::Table(_))
        ));
    }
}
