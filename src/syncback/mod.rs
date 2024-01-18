mod file_names;
mod fs_snapshot;
mod snapshot;

use anyhow::Context;
use memofs::Vfs;
use rbx_dom_weak::{types::Ref, Instance, WeakDom};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

use crate::{
    glob::Glob,
    snapshot::{hash_tree, InstanceSnapshot, InstanceWithMeta, RojoTree},
    snapshot_middleware::Middleware,
    Project,
};

pub use file_names::{is_valid_file_name, name_for_inst};
pub use fs_snapshot::FsSnapshot;
pub use snapshot::{SyncbackData, SyncbackSnapshot};

pub fn syncback_loop<'old>(
    vfs: &'old Vfs,
    old_tree: &'old RojoTree,
    new_tree: &WeakDom,
    project: &'old Project,
) -> anyhow::Result<Vec<(Ref, InstanceSnapshot)>> {
    log::debug!("Hashing project DOM");
    let old_hashes = hash_tree(old_tree.inner());
    log::debug!("Hashing file DOM");
    let new_hashes = hash_tree(new_tree);

    let project_path = project.folder_location();

    let syncback_data = SyncbackData {
        vfs,
        old_tree,
        new_tree,
        syncback_rules: project.syncback_rules.as_ref(),
    };

    let mut snapshots = vec![SyncbackSnapshot {
        data: syncback_data,
        old: Some(old_tree.get_root_id()),
        new: new_tree.root_ref(),
        parent_path: project.folder_location().to_path_buf(),
        name: project.name.clone(),
    }];

    let mut replacements = Vec::new();
    let mut fs_snapshot = FsSnapshot::new();

    'syncback: while let Some(snapshot) = snapshots.pop() {
        let inst_path = get_inst_path(new_tree, snapshot.new);
        // We can quickly check that two subtrees are identical and if they are,
        // skip reconciling them.
        if let Some(old_ref) = snapshot.old {
            if old_hashes.get(&old_ref) == new_hashes.get(&snapshot.new) {
                log::trace!(
                    "Skipping {inst_path} due to it being identically hashed as {:?}",
                    old_hashes.get(&old_ref)
                );
                continue;
            }
        }

        let middleware = snapshot
            .old_inst()
            .and_then(|inst| inst.metadata().middleware)
            .unwrap_or_else(|| get_best_middleware(snapshot.new_inst()));
        log::trace!("Middleware for {inst_path} is {:?}", middleware);

        if matches!(middleware, Middleware::Json | Middleware::Toml) {
            log::warn!("Cannot syncback {middleware:?} at {inst_path}, skipping");
            continue;
        }

        let appended_name = name_for_inst(middleware, snapshot.new_inst(), snapshot.old_inst())?;
        let working_path = snapshot.parent_path.join(appended_name.as_ref());

        if !snapshot.is_valid_path(project_path, &working_path) {
            log::debug!("Skipping {inst_path} because its path matches ignore pattern");
            continue;
        }

        if let Some(syncback_rules) = &project.syncback_rules {
            // Ignore trees;
            for ignored in &syncback_rules.ignore_trees {
                if inst_path.starts_with(ignored.as_str()) {
                    log::debug!("Tree {inst_path} is blocked by project");
                    continue 'syncback;
                }
            }
        }

        let syncback = middleware
            .syncback(&snapshot)
            .with_context(|| format!("Failed to syncback {inst_path}"))?;

        if !syncback.removed_children.is_empty() {
            log::debug!(
                "removed children for {inst_path}: {}",
                syncback.removed_children.len()
            );
            for inst in &syncback.removed_children {
                let path = inst.metadata().instigating_source.as_ref().unwrap().path();
                if path.is_dir() {
                    fs_snapshot.remove_dir(path)
                } else {
                    fs_snapshot.remove_file(path)
                }
            }
        }

        if let Some(old_inst) = snapshot.old_inst() {
            replacements.push((old_inst.parent(), syncback.inst_snapshot));
        }

        fs_snapshot.merge(syncback.fs_snapshot);

        snapshots.extend(syncback.children);
    }

    fs_snapshot.write_to_vfs(project.folder_location(), vfs)?;

    Ok(replacements)
}

pub struct SyncbackReturn<'new, 'old> {
    pub inst_snapshot: InstanceSnapshot,
    pub fs_snapshot: FsSnapshot,
    pub children: Vec<SyncbackSnapshot<'new, 'old>>,
    pub removed_children: Vec<InstanceWithMeta<'old>>,
}

pub fn get_best_middleware(inst: &Instance) -> Middleware {
    match inst.class.as_str() {
        "Folder" | "Configuration" | "Tool" | "ScreenGui" => Middleware::Dir,
        "Sound"
        | "SoundGroup"
        | "Sky"
        | "Atmosphere"
        | "BloomEffect"
        | "BlurEffect"
        | "ColorCorrectionEffect"
        | "DepthOfFieldEffect"
        | "SunRaysEffect" => {
            if inst.children().is_empty() {
                Middleware::JsonModel
            } else {
                // This begs the question of an init.model.json but we'll leave
                // that for another day.
                Middleware::Dir
            }
        }
        "StringValue" => {
            if inst.children().is_empty() {
                Middleware::Text
            } else {
                Middleware::Dir
            }
        }
        "Script" => {
            if inst.children().is_empty() {
                Middleware::ServerScript
            } else {
                Middleware::ServerScriptDir
            }
        }
        "LocalScript" => {
            if inst.children().is_empty() {
                Middleware::ClientScript
            } else {
                Middleware::ClientScriptDir
            }
        }
        "ModuleScript" => {
            if inst.children().is_empty() {
                Middleware::ModuleScript
            } else {
                Middleware::ModuleScriptDir
            }
        }
        "LocalizationTable" => {
            if inst.children().is_empty() {
                Middleware::Csv
            } else {
                Middleware::CsvDir
            }
        }
        _ => Middleware::Rbxm,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SyncbackRules {
    /// A list of subtrees in a file that will be ignored by Syncback.
    #[serde(default)]
    ignore_trees: Vec<String>,
    /// A list of patterns to check against the path an Instance would serialize
    /// to. If a path matches one of these, the Instance won't be syncbacked.
    #[serde(default)]
    ignore_paths: Vec<Glob>,
    /// A map of classes to properties to ignore for that class when doing
    /// syncback.
    #[serde(default)]
    ignore_properties: HashMap<String, Vec<String>>,
}

fn get_inst_path(dom: &WeakDom, referent: Ref) -> String {
    let mut path: VecDeque<&str> = VecDeque::new();
    let mut inst = dom.get_by_ref(referent);
    while let Some(instance) = inst {
        path.push_front(&instance.name);
        inst = dom.get_by_ref(instance.parent());
    }
    path.into_iter().collect::<Vec<&str>>().join("/")
}
