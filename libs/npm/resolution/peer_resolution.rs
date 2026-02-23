// Copyright 2018-2026 the Deno authors. MIT license.

//! Phase 2 of the two-phase npm dependency resolution.
//!
//! Walks the frozen dependency tree from Phase 1 via DFS, resolving
//! peer dependencies by looking up "parent packages" (packages visible
//! from ancestors in the tree). Produces `NpmPackageId`s with peer
//! dependencies encoded, and builds the final `NpmResolutionSnapshot`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;

use deno_semver::StackString;
use deno_semver::VersionReq;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReq;
use indexmap::IndexSet;

use super::dep_tree::DepTree;
use super::dep_tree::DepTreeNodeId;
use super::graph::UnmetPeerDepDiagnostic;
use super::snapshot::NpmResolutionSnapshot;
use super::snapshot::SnapshotPackageCopyIndexResolver;
use crate::NpmPackageExtraInfo;
use crate::NpmPackageId;
use crate::NpmPackageIdPeerDependencies;
use crate::NpmResolutionPackage;
use crate::NpmResolutionPackageSystemInfo;
use crate::registry::NpmDependencyEntryKind;

/// Result of resolving peers for a single tree node.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedNodePeers {
  /// The final NpmPackageId with peers encoded
  pkg_id: NpmPackageId,
  /// Full dependency map including resolved peers: specifier → NpmPackageId
  dependencies: HashMap<StackString, NpmPackageId>,
  /// Which specifiers are optional dependencies
  optional_dependencies: HashSet<StackString>,
  /// Which specifiers are optional peer dependencies
  optional_peer_dependencies: HashSet<StackString>,
  /// All resolved peers (own + bubbled from children) — keyed by package name.
  /// Used by the parent to propagate peers upward through the tree.
  all_resolved_peers: HashMap<StackString, NpmPackageId>,
}

/// Result of Phase 2.
pub struct PeerResolutionResult {
  /// All resolved nodes. A single DepTreeNodeId may appear multiple times
  /// with different peer contexts (e.g., a shared dep node under two parents
  /// with different peer versions).
  pub(crate) all_resolved: Vec<(DepTreeNodeId, ResolvedNodePeers)>,
  /// Maps root package nv node_id → resolved peers (for root_packages mapping).
  pub(crate) root_resolved: HashMap<DepTreeNodeId, ResolvedNodePeers>,
  /// Diagnostics for unmet required peer deps.
  pub unmet_peer_diagnostics: Vec<UnmetPeerDepDiagnostic>,
}

/// Packages visible to a node for peer resolution.
///
/// Built up as we descend the tree — each level adds its siblings
/// (i.e., the parent's children) so that peer deps can find them.
#[derive(Debug, Clone)]
struct ParentPackages {
  /// name → (nv, DepTreeNodeId)
  pkgs: HashMap<StackString, (Rc<PackageNv>, DepTreeNodeId)>,
}

impl ParentPackages {
  fn new() -> Self {
    Self {
      pkgs: HashMap::new(),
    }
  }

  /// Create a new ParentPackages extended with the children of the given node.
  fn extended_with(
    &self,
    tree: &DepTree,
    node_id: DepTreeNodeId,
  ) -> Self {
    let mut pkgs = self.pkgs.clone();
    let node = tree.get_node(node_id);
    // Add this node's regular children as visible packages
    for (_specifier, &child_id) in &node.children {
      let child = tree.get_node(child_id);
      pkgs.insert(child.nv.name.clone(), (child.nv.clone(), child_id));
    }
    // Also add the node itself
    pkgs.insert(node.nv.name.clone(), (node.nv.clone(), node_id));
    Self { pkgs }
  }

  /// Find a package by name that satisfies the given version req.
  fn find(
    &self,
    name: &str,
    _version_req: &VersionReq,
  ) -> Option<(Rc<PackageNv>, DepTreeNodeId)> {
    // For peer deps, we match by name. The version compatibility
    // check is done by the caller if needed.
    self.pkgs.get(name).cloned()
  }
}

/// Context for the peer resolution DFS.
struct PeerResolutionCtx<'a> {
  tree: &'a DepTree,
  /// All resolved results — a node may appear multiple times with different
  /// peer contexts.
  all_results: Vec<(DepTreeNodeId, ResolvedNodePeers)>,
  /// Cache: (nv, sorted peer pkg names) → ResolvedNodePeers
  /// Avoids re-resolving the same node with the same peer context
  peers_cache: HashMap<(Rc<PackageNv>, Vec<StackString>), ResolvedNodePeers>,
  /// Diagnostics
  unmet_peer_diagnostics: IndexSet<UnmetPeerDepDiagnostic>,
}

/// Run Phase 2 peer resolution on the frozen dep tree.
pub fn resolve_peers(tree: &DepTree) -> PeerResolutionResult {
  let mut ctx = PeerResolutionCtx {
    tree,
    all_results: Vec::with_capacity(tree.nodes.len()),
    peers_cache: HashMap::new(),
    unmet_peer_diagnostics: IndexSet::new(),
  };

  let root_parent_pkgs = {
    let mut pkgs = ParentPackages::new();
    // Root-level visible packages include all root packages
    for (nv, &node_id) in &tree.root_packages {
      pkgs.pkgs.insert(nv.name.clone(), (nv.clone(), node_id));
    }
    pkgs
  };

  for (_nv, &node_id) in &tree.root_packages {
    let _ = resolve_peers_of_node(
      node_id,
      &root_parent_pkgs,
      &mut ctx,
      &mut vec![],
      &[],
    );
  }

  // Fix up cycle back-edges: when a cycle was detected during DFS,
  // the dependency got a bare NpmPackageId (no peers). Now that resolution
  // is complete, replace these bare IDs with the actual resolved IDs.
  // For each root package, find the best resolution from all_results.
  // A root package that also appears as a peer dep in a deeper context
  // may have a better resolution (with more peer deps) than the root-level one.
  // Use the root-level resolution (first entry) by default, and only
  // replace it if a deeper resolution has strictly MORE peer deps.
  let mut root_resolved = HashMap::with_capacity(tree.root_packages.len());
  for (_nv, &node_id) in &tree.root_packages {
    let mut best: Option<&ResolvedNodePeers> = None;
    for (id, resolved) in ctx.all_results.iter() {
      if *id != node_id {
        continue;
      }
      match &best {
        None => best = Some(resolved),
        Some(current_best) => {
          if resolved.pkg_id.peer_dependencies.iter().count()
            > current_best.pkg_id.peer_dependencies.iter().count()
          {
            best = Some(resolved);
          }
        }
      }
    }
    if let Some(resolved) = best {
      root_resolved.insert(node_id, resolved.clone());
    }
  }

  let mut result = PeerResolutionResult {
    all_resolved: ctx.all_results,
    root_resolved,
    unmet_peer_diagnostics: ctx.unmet_peer_diagnostics.into_iter().collect(),
  };

  dedupe_peer_dependents(&mut result);

  result
}

/// Return type for resolve_peers_of_node: the resolved NpmPackageId plus
/// peers that should bubble up to ancestor nodes (keyed by package name).
struct NodePeerResult {
  pkg_id: NpmPackageId,
  /// Peers that should continue bubbling to the parent.
  /// Keyed by package name for dedup and filtering.
  bubbling_peers: HashMap<StackString, NpmPackageId>,
}

/// Recursively resolve peers for a node and its subtree.
///
/// Returns the resolved NpmPackageId plus any peer deps that should
/// propagate upward through ancestor identities.
fn resolve_peers_of_node(
  node_id: DepTreeNodeId,
  parent_pkgs: &ParentPackages,
  ctx: &mut PeerResolutionCtx,
  ancestor_path: &mut Vec<DepTreeNodeId>,
  ancestor_nvs: &[Rc<PackageNv>],
) -> NodePeerResult {
  let is_root_level = ancestor_nvs.is_empty();
  let node = ctx.tree.get_node(node_id);
  let nv = node.nv.clone();

  // Check cache: same nv + same visible peer package set → same result.
  // This correctly handles shared nodes that appear in different peer
  // contexts (they'll have different cache keys).
  let cache_key = make_cache_key(node_id, parent_pkgs, ctx.tree);
  if let Some(cached) = ctx.peers_cache.get(&cache_key) {
    let result = cached.clone();
    let bubbling = result.all_resolved_peers.clone();
    ctx.all_results.push((node_id, result));
    return NodePeerResult {
      pkg_id: ctx.all_results.last().unwrap().1.pkg_id.clone(),
      bubbling_peers: bubbling,
    };
  }

  // Detect cycles: if this node is already in the ancestor path,
  // return a truncated ID to break the cycle
  if ancestor_path.contains(&node_id) {
    return NodePeerResult {
      pkg_id: NpmPackageId {
        nv: (*nv).clone(),
        peer_dependencies: Default::default(),
      },
      bubbling_peers: HashMap::new(),
    };
  }

  ancestor_path.push(node_id);

  // Build parent_pkgs for children: current parent_pkgs + this node's children
  let child_parent_pkgs = parent_pkgs.extended_with(ctx.tree, node_id);

  // Collect the names of this node's direct children (for filtering later)
  let children: Vec<_> = node
    .children
    .iter()
    .map(|(k, v)| (k.clone(), *v))
    .collect();
  let child_pkg_names: HashSet<StackString> = children
    .iter()
    .map(|(_, child_id)| ctx.tree.get_node(*child_id).nv.name.clone())
    .collect();

  // All resolved peers for this node (own + bubbled from children).
  // This will be included in the NpmPackageId and stored for parent use.
  let mut all_resolved_peers: HashMap<StackString, NpmPackageId> =
    HashMap::new();

  // First, recurse into regular children
  let mut dependencies = HashMap::with_capacity(
    node.children.len() + node.peer_dep_specifiers.len(),
  );

  for (specifier, child_id) in &children {
    let mut child_ancestor_nvs = ancestor_nvs.to_vec();
    child_ancestor_nvs.push(nv.clone());
    let child_result = resolve_peers_of_node(
      *child_id,
      &child_parent_pkgs,
      ctx,
      ancestor_path,
      &child_ancestor_nvs,
    );
    dependencies.insert(specifier.clone(), child_result.pkg_id);

    // Collect bubbling peers from this child
    for (peer_name, peer_id) in child_result.bubbling_peers {
      all_resolved_peers.entry(peer_name).or_insert(peer_id);
    }
  }

  // Now resolve peer deps by searching parent_pkgs
  let optional_peer_dep_specifiers = node.optional_peer_dep_specifiers.clone();
  let deps = node.deps.clone();

  for dep in deps.iter() {
    if !matches!(
      dep.kind,
      NpmDependencyEntryKind::Peer | NpmDependencyEntryKind::OptionalPeer
    ) {
      continue;
    }

    let specifier = &dep.bare_specifier;

    // Try to find in parent packages
    if let Some((peer_nv, peer_node_id)) =
      parent_pkgs.find(&dep.name, &dep.version_req)
    {
      // Found a matching peer in the parent context.
      // Check if the version satisfies the requirement — if not, emit
      // an unmet peer dep diagnostic (but still resolve it).
      // Skip diagnostics at root level since the package may be properly
      // resolved in a different context deeper in the tree.
      if !is_root_level
        && dep.version_req.tag().is_none()
        && !dep.version_req.matches(&peer_nv.version)
      {
        // Build ancestors in leaf-to-root order: current node first,
        // then parent, grandparent, etc.
        let mut ancestors_for_diagnostic: Vec<PackageNv> =
          vec![(*nv).clone()];
        for anv in ancestor_nvs.iter().rev() {
          ancestors_for_diagnostic.push((**anv).clone());
        }
        ctx.unmet_peer_diagnostics.insert(UnmetPeerDepDiagnostic {
          ancestors: ancestors_for_diagnostic,
          dependency: PackageReq {
            name: dep.name.clone(),
            version_req: dep.version_req.clone(),
          },
          resolved: peer_nv.version.clone(),
        });
      }

      let mut peer_ancestor_nvs = ancestor_nvs.to_vec();
      peer_ancestor_nvs.push(nv.clone());
      let peer_result = resolve_peers_of_node(
        peer_node_id,
        &child_parent_pkgs,
        ctx,
        ancestor_path,
        &peer_ancestor_nvs,
      );
      dependencies.insert(specifier.clone(), peer_result.pkg_id.clone());
      // Add this peer to our resolved peers set
      all_resolved_peers
        .insert(dep.name.clone(), peer_result.pkg_id);
      // Also collect the peer's own bubbling peers (transitive peer deps)
      for (peer_name, peer_id) in peer_result.bubbling_peers {
        all_resolved_peers.entry(peer_name).or_insert(peer_id);
      }
    } else if !is_root_level
      && !optional_peer_dep_specifiers.contains(specifier)
    {
      // Required peer not found — record diagnostic.
      // Skip at root level since the package may be resolved in a
      // different context deeper in the tree.
      // Build ancestors in leaf-to-root order.
      let mut ancestors_for_diagnostic: Vec<PackageNv> =
        vec![(*nv).clone()];
      for anv in ancestor_nvs.iter().rev() {
        ancestors_for_diagnostic.push((**anv).clone());
      }

      ctx.unmet_peer_diagnostics.insert(UnmetPeerDepDiagnostic {
        ancestors: ancestors_for_diagnostic,
        dependency: PackageReq {
          name: dep.name.clone(),
          version_req: dep.version_req.clone(),
        },
        resolved: nv.version.clone(),
      });
    }
    // Optional peer not found — that's OK, skip it
  }

  // Build NpmPackageId with ALL resolved peers (own + bubbled from children),
  // excluding self-references (a package can't be its own peer dep).
  // Remove self from the peer set first.
  all_resolved_peers.remove(nv.name.as_str());

  let mut peer_dependencies = NpmPackageIdPeerDependencies::with_capacity(
    all_resolved_peers.len(),
  );
  let mut seen_peer_ids = HashSet::new();
  // Sort by name for deterministic ordering
  let mut sorted_peers: Vec<_> = all_resolved_peers.iter().collect();
  sorted_peers.sort_by(|(a, _), (b, _)| a.cmp(b));
  for (_name, peer_id) in &sorted_peers {
    if seen_peer_ids.insert((*peer_id).clone()) {
      peer_dependencies.push((*peer_id).clone());
    }
  }

  let pkg_id = NpmPackageId {
    nv: (*nv).clone(),
    peer_dependencies,
  };

  // Compute bubbling peers: all_resolved_peers minus those whose name
  // matches a direct child of this node. Peers that match a direct child
  // are "consumed" here and don't propagate further up.
  let bubbling_peers: HashMap<StackString, NpmPackageId> = all_resolved_peers
    .iter()
    .filter(|(name, _)| !child_pkg_names.contains(name.as_str()))
    .map(|(k, v)| (k.clone(), v.clone()))
    .collect();

  // Collect optional dependencies (from version info)
  let version_info = &node.version_info;
  let optional_dependencies: HashSet<StackString> = version_info
    .optional_dependencies
    .keys()
    .cloned()
    .collect();

  let result = ResolvedNodePeers {
    pkg_id: pkg_id.clone(),
    dependencies,
    optional_dependencies,
    optional_peer_dependencies: optional_peer_dep_specifiers,
    all_resolved_peers,
  };

  ctx.all_results.push((node_id, result.clone()));
  ctx.peers_cache.insert(cache_key, result);

  ancestor_path.pop();

  NodePeerResult {
    pkg_id,
    bubbling_peers,
  }
}

/// Build a cache key for peer resolution memoization.
fn make_cache_key(
  node_id: DepTreeNodeId,
  parent_pkgs: &ParentPackages,
  tree: &DepTree,
) -> (Rc<PackageNv>, Vec<StackString>) {
  let node = tree.get_node(node_id);
  let mut peer_names: Vec<StackString> = node
    .peer_dep_specifiers
    .iter()
    .filter_map(|spec| {
      // Find the dep entry for this specifier
      node.deps.iter().find(|d| d.bare_specifier == *spec)
    })
    .filter_map(|dep| {
      parent_pkgs
        .find(&dep.name, &dep.version_req)
        .map(|(nv, _)| {
          StackString::from_string(format!("{}@{}", nv.name, nv.version))
        })
    })
    .collect();
  peer_names.sort();
  (node.nv.clone(), peer_names)
}

// ======================================================================
// Peer dependent deduplication (mirrors pnpm's `dedupePeerDependents`)
// ======================================================================

/// Deduplicate peer-dependent copies of the same package.
///
/// When the same package (same nv) is resolved with different peer dep
/// sets in different contexts, a copy with fewer peers can be merged
/// into a copy with more peers if the larger is a strict superset.
///
/// For example, `vite@6.2.4` (bare) and `vite@6.2.4_lightningcss@1.29.2`
/// can be merged because the latter has all of the former's deps (none)
/// plus lightningcss. After merging, all references to bare vite are
/// replaced with the lightningcss variant.
pub fn dedupe_peer_dependents(result: &mut PeerResolutionResult) {
  loop {
    let merge_map = find_peer_dep_merges(&result.all_resolved);
    if merge_map.is_empty() {
      break;
    }
    apply_merge_map(&mut result.all_resolved, &merge_map);
    apply_merge_map_entries(&mut result.root_resolved, &merge_map);
  }
}

/// Count of deps for the superset check, matching pnpm's `nodeDepsCount`.
fn deps_count(resolved: &ResolvedNodePeers) -> usize {
  resolved.dependencies.len() + resolved.all_resolved_peers.len()
}

/// Check if `superset` is a compatible superset of `subset`.
/// Both must be for the same `PackageNv`.
///
/// Mirrors pnpm's `isCompatibleAndHasMoreDeps`:
/// 1. superset has >= deps than subset
/// 2. All of subset's dependency VALUES exist in superset's dependency values
/// 3. All of subset's resolved peer NAMES exist in superset's resolved peers
fn is_compatible_superset(
  superset: &ResolvedNodePeers,
  subset: &ResolvedNodePeers,
) -> bool {
  if deps_count(superset) < deps_count(subset) {
    return false;
  }

  // All subset dependency values must exist in superset dependency values
  let superset_dep_values: HashSet<&NpmPackageId> =
    superset.dependencies.values().collect();
  for dep_value in subset.dependencies.values() {
    if !superset_dep_values.contains(dep_value) {
      return false;
    }
  }

  // All subset resolved peer names must exist in superset
  for peer_name in subset.all_resolved_peers.keys() {
    if !superset.all_resolved_peers.contains_key(peer_name) {
      return false;
    }
  }

  true
}

/// Find merge opportunities among all resolved entries.
///
/// Groups by `PackageNv`, then for each group with multiple distinct
/// `NpmPackageId`s, tries to merge smaller copies into larger ones
/// using pnpm's greedy algorithm.
fn find_peer_dep_merges(
  all_resolved: &[(DepTreeNodeId, ResolvedNodePeers)],
) -> HashMap<NpmPackageId, NpmPackageId> {
  // Group by PackageNv → unique NpmPackageId → representative ResolvedNodePeers
  let mut by_nv: HashMap<&PackageNv, HashMap<&NpmPackageId, &ResolvedNodePeers>> =
    HashMap::new();
  for (_, resolved) in all_resolved {
    by_nv
      .entry(&resolved.pkg_id.nv)
      .or_default()
      .entry(&resolved.pkg_id)
      .or_insert(resolved);
  }

  let mut merge_map = HashMap::new();

  for unique_by_id in by_nv.values() {
    if unique_by_id.len() <= 1 {
      continue;
    }

    // Sort by dep count ascending
    let mut sorted: Vec<&ResolvedNodePeers> =
      unique_by_id.values().copied().collect();
    sorted.sort_by_key(|r| deps_count(r));

    // pnpm algorithm: pop largest, try to merge all remaining into it
    while sorted.len() > 1 {
      let largest = sorted.pop().unwrap();
      let mut next = vec![];
      while let Some(smaller) = sorted.pop() {
        if is_compatible_superset(largest, smaller) {
          merge_map
            .insert(smaller.pkg_id.clone(), largest.pkg_id.clone());
        } else {
          next.push(smaller);
        }
      }
      sorted = next;
      sorted.sort_by_key(|r| deps_count(r));
    }
  }

  merge_map
}

/// Recursively apply the merge map to an `NpmPackageId`, including
/// nested peer_dependencies.
fn apply_merge_to_id(
  id: &NpmPackageId,
  merge_map: &HashMap<NpmPackageId, NpmPackageId>,
) -> NpmPackageId {
  // Check if this exact ID is in the merge map
  if let Some(new_id) = merge_map.get(id) {
    return new_id.clone();
  }

  // Recursively apply to peer_dependencies
  let peer_count = id.peer_dependencies.iter().count();
  if peer_count == 0 {
    return id.clone();
  }

  let mut new_peers =
    NpmPackageIdPeerDependencies::with_capacity(peer_count);
  let mut changed = false;
  for peer in id.peer_dependencies.iter() {
    let new_peer = apply_merge_to_id(peer, merge_map);
    if new_peer != *peer {
      changed = true;
    }
    new_peers.push(new_peer);
  }

  if changed {
    let result = NpmPackageId {
      nv: id.nv.clone(),
      peer_dependencies: new_peers,
    };
    // Check if the resulting ID itself should be merged
    merge_map.get(&result).cloned().unwrap_or(result)
  } else {
    id.clone()
  }
}

/// Apply the merge map to all NpmPackageIds in all_resolved entries.
fn apply_merge_map(
  all_resolved: &mut [(DepTreeNodeId, ResolvedNodePeers)],
  merge_map: &HashMap<NpmPackageId, NpmPackageId>,
) {
  for (_, resolved) in all_resolved.iter_mut() {
    resolved.pkg_id = apply_merge_to_id(&resolved.pkg_id, merge_map);
    for dep_id in resolved.dependencies.values_mut() {
      *dep_id = apply_merge_to_id(dep_id, merge_map);
    }
    for peer_id in resolved.all_resolved_peers.values_mut() {
      *peer_id = apply_merge_to_id(peer_id, merge_map);
    }
  }
}

/// Apply the merge map to all NpmPackageIds in a HashMap of entries.
fn apply_merge_map_entries(
  entries: &mut HashMap<DepTreeNodeId, ResolvedNodePeers>,
  merge_map: &HashMap<NpmPackageId, NpmPackageId>,
) {
  for resolved in entries.values_mut() {
    resolved.pkg_id = apply_merge_to_id(&resolved.pkg_id, merge_map);
    for dep_id in resolved.dependencies.values_mut() {
      *dep_id = apply_merge_to_id(dep_id, merge_map);
    }
    for peer_id in resolved.all_resolved_peers.values_mut() {
      *peer_id = apply_merge_to_id(peer_id, merge_map);
    }
  }
}

/// Build the final `NpmResolutionSnapshot` from the dep tree and
/// peer resolution results.
pub fn build_snapshot(
  tree: &DepTree,
  peer_result: &PeerResolutionResult,
) -> NpmResolutionSnapshot {
  use std::collections::VecDeque;

  let mut package_reqs = HashMap::with_capacity(tree.package_reqs.len());
  let mut root_packages = HashMap::with_capacity(tree.root_packages.len());
  let mut packages_by_name =
    HashMap::<StackString, Vec<NpmPackageId>>::with_capacity(tree.nodes.len());
  let mut packages =
    HashMap::<NpmPackageId, NpmResolutionPackage>::with_capacity(
      tree.nodes.len(),
    );

  let mut copy_index_resolver =
    SnapshotPackageCopyIndexResolver::with_capacity(tree.nodes.len());

  // Build a lookup from NpmPackageId → (DepTreeNodeId, ResolvedNodePeers)
  // for all resolved nodes. When multiple entries share the same pkg_id
  // (e.g., after dedupe_peer_dependents merging), prefer the entry with
  // more dependencies — it's the superset that others were merged into.
  let mut resolved_by_pkg_id =
    HashMap::<NpmPackageId, (DepTreeNodeId, &ResolvedNodePeers)>::with_capacity(
      peer_result.all_resolved.len(),
    );
  for (node_id, resolved) in &peer_result.all_resolved {
    match resolved_by_pkg_id.entry(resolved.pkg_id.clone()) {
      std::collections::hash_map::Entry::Occupied(mut existing) => {
        if resolved.dependencies.len()
          > existing.get().1.dependencies.len()
        {
          *existing.get_mut() = (*node_id, resolved);
        }
      }
      std::collections::hash_map::Entry::Vacant(v) => {
        v.insert((*node_id, resolved));
      }
    }
  }


  // Set up root packages mapping.
  // Only include packages that have a corresponding package_req — auto-resolved
  // peer deps are in tree.root_packages for Phase 2 visibility but should not
  // appear as top-level packages in the snapshot.
  let package_req_nvs: HashSet<&PackageNv> =
    tree.package_reqs.values().map(|nv| nv.as_ref()).collect();
  for (req, nv) in &tree.package_reqs {
    package_reqs.insert(req.clone(), (**nv).clone());
  }
  for (nv, &node_id) in &tree.root_packages {
    if !package_req_nvs.contains(nv.as_ref()) {
      continue; // Skip auto-resolved peer deps
    }
    if let Some(resolved) = peer_result.root_resolved.get(&node_id) {
      root_packages.insert((**nv).clone(), resolved.pkg_id.clone());
    }
  }

  // DFS from root packages to collect only reachable packages.
  // This avoids including incomplete resolutions (e.g., root-level
  // resolutions where peer deps weren't found but were resolved
  // in a deeper context).
  let mut traversed_ids = HashSet::with_capacity(peer_result.all_resolved.len());
  let mut pending = VecDeque::new();

  // Use sorted root_packages for deterministic copy_index assignment
  let mut sorted_roots: Vec<_> = root_packages.iter().collect();
  sorted_roots.sort_by_key(|(nv, _)| (*nv).clone());
  for (_, root_pkg_id) in sorted_roots {
    if traversed_ids.insert(root_pkg_id.clone()) {
      pending.push_back(root_pkg_id.clone());
    }
  }

  while let Some(pkg_id) = pending.pop_front() {
    let Some(&(node_id, resolved)) = resolved_by_pkg_id.get(&pkg_id) else {
      continue;
    };

    // Queue dependencies for traversal
    for dep_pkg_id in resolved.dependencies.values() {
      if traversed_ids.insert(dep_pkg_id.clone()) {
        pending.push_back(dep_pkg_id.clone());
      }
    }

    let node = tree.get_node(node_id);
    let copy_index = copy_index_resolver.resolve(&pkg_id);

    let version_info = &node.version_info;
    let system = NpmResolutionPackageSystemInfo {
      os: version_info.os.clone(),
      cpu: version_info.cpu.clone(),
    };

    packages_by_name
      .entry(pkg_id.nv.name.clone())
      .or_default()
      .push(pkg_id.clone());

    packages.insert(
      pkg_id.clone(),
      NpmResolutionPackage {
        id: pkg_id.clone(),
        copy_index,
        system,
        dist: version_info.dist.clone(),
        dependencies: resolved.dependencies.clone(),
        optional_dependencies: resolved.optional_dependencies.clone(),
        optional_peer_dependencies: resolved
          .optional_peer_dependencies
          .clone(),
        extra: Some(NpmPackageExtraInfo {
          bin: version_info.bin.clone(),
          scripts: version_info.scripts.clone(),
          deprecated: version_info.deprecated.clone(),
        }),
        is_deprecated: version_info.deprecated.is_some(),
        has_bin: version_info.bin.is_some(),
        has_scripts: version_info.scripts.contains_key("preinstall")
          || version_info.scripts.contains_key("install")
          || version_info.scripts.contains_key("postinstall"),
      },
    );
  }

  NpmResolutionSnapshot {
    package_reqs,
    root_packages,
    packages_by_name,
    packages,
  }
}
