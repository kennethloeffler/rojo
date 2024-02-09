//! Hashing utilities for a WeakDom.
mod variant;

pub use variant::*;

use blake3::{Hash, Hasher};
use rbx_dom_weak::{
    types::{Ref, Variant},
    Instance, WeakDom,
};
use std::collections::{HashMap, VecDeque};

use crate::variant_eq::variant_eq;

/// Returns a map of every `Ref` in the `WeakDom` to a hashed version of the
/// `Instance` it points to, including the properties and descendants of the
/// `Instance`.
///
/// The hashes include the descendants of the Instances in them, so they should
/// only be used for comparing subtrees directly.
pub fn hash_tree(dom: &WeakDom, root_ref: Ref) -> HashMap<Ref, Hash> {
    let mut map: HashMap<Ref, Hash> = HashMap::new();
    let mut order = descendants(dom, root_ref);

    let mut prop_list = Vec::with_capacity(2);

    while let Some(referent) = order.pop() {
        let inst = dom.get_by_ref(referent).unwrap();
        let hash = hash_inst(inst, &mut prop_list, &map);

        map.insert(referent, hash);
    }

    map
}

/// Hashes an Instance using its class, name, and properties. The passed
/// `prop_list` is used to sort properties before hashing them.
fn hash_inst_no_descendants<'inst>(
    inst: &'inst Instance,
    prop_list: &mut Vec<(&'inst str, &'inst Variant)>,
) -> Hasher {
    let mut hasher = Hasher::new();
    hasher.update(inst.class.as_bytes());
    hasher.update(inst.name.as_bytes());

    let descriptor = rbx_reflection_database::get()
        .classes
        .get(inst.class.as_str())
        .expect("class should be known to Rojo");

    for (name, value) in &inst.properties {
        if let Some(default) = descriptor.default_properties.get(name.as_str()) {
            if !variant_eq(default, value) {
                prop_list.push((name, value))
            }
        } else {
            prop_list.push((name, value))
        }
    }

    prop_list.sort_unstable_by_key(|(key, _)| *key);
    for (name, value) in prop_list.iter() {
        hasher.update(name.as_bytes());
        hash_variant(&mut hasher, value)
    }

    prop_list.clear();

    hasher
}

/// Hashes an Instance using its class, name, properties, and descendants.
/// The passed `prop_list` is used to sort properties before hashing them.
///
/// # Panics
/// If any children of the Instance are inside `map`, this function will panic.
fn hash_inst<'inst>(
    inst: &'inst Instance,
    prop_list: &mut Vec<(&'inst str, &'inst Variant)>,
    map: &HashMap<Ref, Hash>,
) -> Hash {
    let mut hasher = hash_inst_no_descendants(inst, prop_list);
    let mut child_list = Vec::with_capacity(inst.children().len());

    for child in inst.children() {
        if let Some(hash) = map.get(child) {
            child_list.push(hash.as_bytes())
        } else {
            panic!("Invariant: child {} not hashed before its parent", child);
        }
    }
    child_list.sort_unstable();
    for hash in child_list {
        hasher.update(hash);
    }

    hasher.finalize()
}

/// Produces a list of descendants in the WeakDom such that all children come
/// before their parents.
fn descendants(dom: &WeakDom, root_ref: Ref) -> Vec<Ref> {
    let mut queue = VecDeque::new();
    let mut ordered = Vec::new();
    queue.push_front(root_ref);

    while let Some(referent) = queue.pop_front() {
        let inst = dom
            .get_by_ref(referent)
            .expect("Invariant: WeakDom had a Ref that wasn't inside it");
        ordered.push(referent);
        for child in inst.children() {
            queue.push_back(*child)
        }
    }

    ordered
}
