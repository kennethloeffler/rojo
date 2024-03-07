use memofs::Vfs;
use rbx_reflection::Scriptability;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use crate::{
    glob::Glob,
    snapshot::{InstanceWithMeta, RojoTree},
    snapshot_middleware::Middleware,
    variant_eq::variant_eq,
};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, WeakDom,
};

use super::SyncbackRules;

/// A glob that can be used to tell if a path contains a `.git` folder.
static GIT_IGNORE_GLOB: OnceLock<Glob> = OnceLock::new();

#[derive(Clone, Copy)]
pub struct SyncbackData<'sync> {
    pub(super) vfs: &'sync Vfs,
    pub(super) old_tree: &'sync RojoTree,
    pub(super) new_tree: &'sync WeakDom,
    pub(super) syncback_rules: Option<&'sync SyncbackRules>,
}

pub struct SyncbackSnapshot<'sync> {
    pub data: SyncbackData<'sync>,
    pub old: Option<Ref>,
    pub new: Ref,
    pub parent_path: PathBuf,
    pub name: String,
    pub middleware: Option<Middleware>,
}

impl<'sync> SyncbackSnapshot<'sync> {
    /// Constructs a SyncbackSnapshot from the provided refs
    /// while inheriting the parent's trees and path
    #[inline]
    pub fn with_parent(&self, new_name: String, new_ref: Ref, old_ref: Option<Ref>) -> Self {
        Self {
            data: self.data,
            old: old_ref,
            new: new_ref,
            parent_path: self.parent_path.join(&self.name),
            name: new_name,
            middleware: None,
        }
    }

    /// Constructs a SyncbackSnapshot from the provided refs and path, while
    /// inheriting this snapshot's trees.
    #[inline]
    pub fn with_new_parent(
        &self,
        new_parent: PathBuf,
        new_name: String,
        new_ref: Ref,
        old_ref: Option<Ref>,
    ) -> Self {
        Self {
            data: self.data,
            old: old_ref,
            new: new_ref,
            parent_path: new_parent,
            name: new_name,
            middleware: None,
        }
    }

    /// Allows a middleware to be 'forced' onto a SyncbackSnapshot to override
    /// the attempts to derive it.
    #[inline]
    pub fn middleware(mut self, middleware: Middleware) -> Self {
        self.middleware = Some(middleware);
        self
    }

    /// Returns a map of properties for the 'new' Instance with filtering
    /// done to avoid noise.
    ///
    /// Note that while the returned map does filter based on the user's
    /// `ignore_props` field, it does not do any other filtering and doesn't
    /// clone any data. This is left to the consumer.
    #[inline]
    pub fn new_filtered_properties(&self) -> HashMap<&'sync str, &'sync Variant> {
        self.get_filtered_properties(self.new, self.old).unwrap()
    }

    /// Returns a map of properties for an Instance from the 'new' tree
    /// with filtering done to avoid noise. Returns `None` only if `new_ref`
    /// instance is not in the new tree or if `old_ref` is provided and not in
    /// the old tree.
    ///
    /// Note that while the returned map does filter based on the user's
    /// `ignore_props` field, it does not do any other filtering and doesn't
    /// clone any data. This is left to the consumer.
    pub fn get_filtered_properties(
        &self,
        new_ref: Ref,
        old_ref: Option<Ref>,
    ) -> Option<HashMap<&'sync str, &'sync Variant>> {
        let inst = self.get_new_instance(new_ref)?;
        let mut properties: HashMap<&str, &Variant> =
            HashMap::with_capacity(inst.properties.capacity());

        let filter = self.get_property_filter();
        let class_data = rbx_reflection_database::get()
            .classes
            .get(inst.class.as_str());

        let predicate = |prop_name: &String, prop_value: &Variant| {
            if matches!(prop_value, Variant::Ref(_) | Variant::SharedString(_)) {
                return true;
            }
            if filter_out_property(inst, prop_name) {
                return true;
            }
            if let Some(list) = &filter {
                if list.contains(prop_name) {
                    return true;
                }
            }
            if !self.sync_unscriptable() {
                if let Some(data) = class_data {
                    if let Some(prop_data) = data.properties.get(prop_name.as_str()) {
                        if matches!(prop_data.scriptability, Scriptability::None) {
                            return true;
                        }
                    }
                }
            }
            false
        };

        if let Some(old_inst) = old_ref.and_then(|referent| self.get_old_instance(referent)) {
            for (name, value) in &inst.properties {
                if predicate(name, value) {
                    continue;
                }
                if old_inst.properties().contains_key(name) {
                    properties.insert(name, value);
                }
            }
        }
        if let Some(class_data) = class_data {
            let defaults = &class_data.default_properties;
            for (name, value) in &inst.properties {
                if predicate(name, value) {
                    continue;
                }
                if let Some(default) = defaults.get(name.as_str()) {
                    if !variant_eq(value, default) {
                        properties.insert(name, value);
                    }
                } else {
                    properties.insert(name, value);
                }
            }
        } else {
            for (name, value) in &inst.properties {
                if predicate(name, value) {
                    continue;
                }
                properties.insert(name, value);
            }
        }

        Some(properties)
    }

    /// Returns a set of properties that should not be written with syncback if
    /// one exists.
    fn get_property_filter(&self) -> Option<BTreeSet<&String>> {
        let filter = self.ignore_props()?;
        let mut set = BTreeSet::new();

        let database = rbx_reflection_database::get();
        let mut current_class_name = self.new_inst().class.as_str();

        loop {
            if let Some(list) = filter.get(current_class_name) {
                set.extend(list)
            }

            let class = database.classes.get(current_class_name)?;
            if let Some(super_class) = class.superclass.as_ref() {
                current_class_name = &super_class;
            } else {
                break;
            }
        }

        Some(set)
    }

    /// Returns whether a given path is allowed for syncback by matching `path`
    /// against every user specified glob for ignoring.
    ///
    /// If the provided `path` is absolute, it has `base_path` stripped from it
    /// to allow globs to operate as if it were local.
    #[inline]
    pub fn is_valid_path(&self, base_path: &Path, path: &Path) -> bool {
        let git_glob = GIT_IGNORE_GLOB.get_or_init(|| Glob::new(".git/**").unwrap());
        let test_path = match path.strip_prefix(base_path) {
            Ok(suffix) => suffix,
            Err(_) => path,
        };
        if git_glob.is_match(test_path) {
            return false;
        }
        if let Some(ignore_paths) = self.ignore_paths() {
            for glob in ignore_paths {
                if glob.is_match(test_path) {
                    return false;
                }
            }
        }
        true
    }

    /// Returns a path to the provided Instance in the new DOM. This path is
    /// where you would look for the object in Roblox Studio.
    #[inline]
    pub fn get_new_inst_path(&self, referent: Ref) -> String {
        let mut path = Vec::new();
        let mut path_capacity = 0;

        let mut inst = self.get_new_instance(referent);
        while let Some(instance) = inst {
            path.push(&instance.name);
            path_capacity += instance.name.len() + 1;
            inst = self.get_new_instance(instance.parent());
        }
        let mut str = String::with_capacity(path_capacity);
        while let Some(segment) = path.pop() {
            str.push_str(segment);
            str.push('/')
        }
        str.pop();

        str
    }

    /// Returns an Instance from the old tree with the provided referent, if it
    /// exists.
    #[inline]
    pub fn get_old_instance(&self, referent: Ref) -> Option<InstanceWithMeta<'sync>> {
        self.data.old_tree.get_instance(referent)
    }

    /// Returns an Instance from the new tree with the provided referent, if it
    /// exists.
    #[inline]
    pub fn get_new_instance(&self, referent: Ref) -> Option<&'sync Instance> {
        self.data.new_tree.get_by_ref(referent)
    }

    /// The 'old' Instance this snapshot is for, if it exists.
    #[inline]
    pub fn old_inst(&self) -> Option<InstanceWithMeta<'sync>> {
        self.old
            .and_then(|old| self.data.old_tree.get_instance(old))
    }

    /// The 'new' Instance this snapshot is for.
    #[inline]
    pub fn new_inst(&self) -> &'sync Instance {
        self.data
            .new_tree
            .get_by_ref(self.new)
            .expect("SyncbackSnapshot should not contain invalid referents")
    }

    /// Returns the underlying VFS being used for syncback.
    #[inline]
    pub fn vfs(&self) -> &'sync Vfs {
        self.data.vfs
    }

    /// Returns the WeakDom used for the 'new' tree.
    #[inline]
    pub fn new_tree(&self) -> &'sync WeakDom {
        self.data.new_tree
    }

    /// Returns user-specified property ignore rules.
    #[inline]
    pub fn ignore_props(&self) -> Option<&HashMap<String, Vec<String>>> {
        self.data
            .syncback_rules
            .map(|rules| &rules.ignore_properties)
    }

    /// Returns user-specified ignore paths.
    #[inline]
    pub fn ignore_paths(&self) -> Option<&[Glob]> {
        self.data
            .syncback_rules
            .map(|rules| rules.ignore_paths.as_slice())
    }

    /// Returns user-specified ignore tree.
    #[inline]
    pub fn ignore_tree(&self) -> Option<&[String]> {
        self.data
            .syncback_rules
            .map(|rules| rules.ignore_trees.as_slice())
    }

    /// Returns the user-defined setting to determine whether unscriptable
    /// properties should be synced back or not.
    #[inline]
    pub fn sync_unscriptable(&self) -> bool {
        self.data
            .syncback_rules
            .and_then(|sr| sr.sync_unscriptable)
            .unwrap_or_default()
    }
}

pub fn filter_out_property(inst: &Instance, prop_name: &str) -> bool {
    match inst.class.as_str() {
        "Script" | "LocalScript" | "ModuleScript" => prop_name == "Source",
        "LocalizationTable" => prop_name == "Contents",
        "StringValue" => prop_name == "Value",
        _ => false,
    }
}
