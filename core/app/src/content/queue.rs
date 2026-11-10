//! Queue requests for content changes. Labels name what changes; they carry
//! no install authority, and the queued mutation still refuses authoritatively.

use super::{
    install::ContentMutations, model::CanonicalId, provenance::RequiredRefusal,
    resolve::ResolutionSelection,
};
use crate::{
    install::model::{InstallQueueContentActionRequest, InstallQueueRequest},
    instances::model::InstanceId,
};

impl ContentMutations {
    /// Selections already recorded are updates; others use the caller's label.
    pub fn install_request(
        &self,
        instance: &InstanceId,
        selections: Vec<ResolutionSelection>,
        allow_incompatible: bool,
        replace_files: Vec<String>,
        label: Option<&str>,
    ) -> InstallQueueRequest {
        let recorded = self.recorded_titles(
            instance,
            selections
                .iter()
                .map(|selection| CanonicalId(selection.canonical_id.clone())),
        );
        let label = if !recorded.is_empty() && recorded.len() == selections.len() {
            queue_label("Update", &recorded, "Update content")
        } else {
            let named = label.and_then(display_text);
            queue_label("Install", named.as_slice(), "Install content")
        };
        InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label,
            action: InstallQueueContentActionRequest::Install {
                selections,
                allow_incompatible,
                replace_files,
            },
        }
    }

    /// Names the enabled content that still needs the targets before queueing.
    pub fn uninstall_request(
        &self,
        instance: &InstanceId,
        canonical_ids: Vec<CanonicalId>,
    ) -> Result<InstallQueueRequest, RequiredRefusal> {
        if let Ok((_, manifest, live)) = self.instance_context(instance) {
            let dependents = manifest.dependents(&live, &canonical_ids);
            if !dependents.is_empty() {
                let targets = canonical_ids
                    .iter()
                    .filter_map(|id| manifest.find(id))
                    .collect::<Vec<_>>();
                return Err(RequiredRefusal::new(&targets, &dependents));
            }
        }
        let names = self.recorded_titles(instance, canonical_ids.iter().cloned());
        Ok(InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label: queue_label("Remove", &names, "Remove content"),
            action: InstallQueueContentActionRequest::Uninstall {
                canonical_ids: canonical_ids.into_iter().map(|id| id.0).collect(),
            },
        })
    }

    /// Selected files of a resolved pack version, applied to an existing instance.
    pub fn pack_files_request(
        instance: &InstanceId,
        canonical_id: CanonicalId,
        version_id: String,
        selected_file_ids: Vec<String>,
        include_overrides: bool,
    ) -> InstallQueueRequest {
        InstallQueueRequest::Content {
            instance_id: instance.to_string(),
            label: "Install modpack files".into(),
            action: InstallQueueContentActionRequest::Modpack {
                canonical_id: canonical_id.0,
                version_id,
                selected_file_ids,
                include_overrides,
            },
        }
    }

    fn recorded_titles(
        &self,
        instance: &InstanceId,
        ids: impl Iterator<Item = CanonicalId>,
    ) -> Vec<String> {
        let Ok(manifest) = self.installed(instance) else {
            return Vec::new();
        };
        ids.filter_map(|id| manifest.find(&id)?.title().and_then(display_text))
            .collect()
    }
}

fn queue_label(verb: &str, names: &[String], fallback: &str) -> String {
    match names {
        [] => fallback.into(),
        [name] => format!("{verb} {name}"),
        [name, rest @ ..] => format!("{verb} {name} and {} more", rest.len()),
    }
}

fn display_text(value: &str) -> Option<String> {
    let text = value
        .split(char::is_control)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.chars().take(96).collect::<String>();
    (!text.is_empty()).then_some(text)
}
