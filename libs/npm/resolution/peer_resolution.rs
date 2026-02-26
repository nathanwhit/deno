// Copyright 2018-2026 the Deno authors. MIT license.

//! Phase 2 of the two-phase npm dependency resolution.
//!
//! Walks the frozen dependency tree from Phase 1 via DFS, resolving
//! peer dependencies by looking up "parent packages" (packages visible
//! from ancestors in the tree). Produces `NpmPackageId`s with peer
//! dependencies encoded, and builds the final `NpmResolutionSnapshot`.

use std::collections::HashMap;
use std::rc::Rc;

use deno_semver::StackString;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use rustc_hash::FxBuildHasher;
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
  dependencies: FxHashMap<StackString, NpmPackageId>,
  /// Dependency specifier → DepTreeNodeId (for post-DFS NpmPackageId reconstruction)
  dep_node_ids: FxHashMap<StackString, DepTreeNodeId>,
  /// Which specifiers are optional dependencies
  optional_dependencies: FxHashSet<StackString>,
  /// Which specifiers are optional peer dependencies
  optional_peer_dependencies: FxHashSet<StackString>,
  /// All resolved peers (own + bubbled from children) — keyed by package name.
  /// Used by the parent to propagate peers upward through the tree.
  all_resolved_peers: FxHashMap<StackString, NpmPackageId>,
  /// Ordered resolved peer node IDs for post-DFS NpmPackageId reconstruction.
  /// Preserves insertion order: own peers first, then bubbled from children.
  all_resolved_peer_node_ids: Vec<(StackString, DepTreeNodeId)>,
}

/// Result of Phase 2.
pub struct PeerResolutionResult {
  /// All resolved nodes. A single DepTreeNodeId may appear multiple times
  /// with different peer contexts (e.g., a shared dep node under two parents
  /// with different peer versions).
  pub(crate) all_resolved: Vec<(DepTreeNodeId, ResolvedNodePeers)>,
  /// Maps root package nv node_id → resolved peers (for root_packages mapping).
  pub(crate) root_resolved: FxHashMap<DepTreeNodeId, ResolvedNodePeers>,
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
  pkgs: FxHashMap<StackString, (Rc<PackageNv>, DepTreeNodeId)>,
}

impl ParentPackages {
  fn new() -> Self {
    Self {
      pkgs: FxHashMap::default(),
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
  /// Cache: (node_id, sorted parent node IDs) → ResolvedNodePeers
  /// Avoids re-resolving the same node with the same peer context
  peers_cache: HashMap<(DepTreeNodeId, Vec<DepTreeNodeId>), ResolvedNodePeers, FxBuildHasher>,
  /// Diagnostics
  unmet_peer_diagnostics: IndexSet<UnmetPeerDepDiagnostic>,
  /// NVs of auto-resolved peer deps (in root_packages but not in package_reqs).
  /// These are placed at the root for visibility during Phase 2, but should not
  /// bubble up through the tree to affect ancestor identities — matching the v1
  /// behavior where auto-resolved peers are placed as local children of the
  /// requesting node rather than at the root.
  auto_resolved_nvs: FxHashSet<PackageNv>,
  /// Partially resolved peer node IDs (own peers only, before children).
  /// Used for cycle back-edge bubbling so that ancestors' own peers
  /// propagate correctly even during cycles.
  partial_peer_node_ids: FxHashMap<DepTreeNodeId, Vec<(StackString, DepTreeNodeId)>>,
}

/// Run Phase 2 peer resolution on the frozen dep tree.
pub fn resolve_peers(tree: &DepTree) -> PeerResolutionResult {
  // Compute auto-resolved NVs: packages in root_packages but not in package_reqs.
  let package_req_nvs: FxHashSet<&PackageNv> = tree
    .package_reqs
    .values()
    .map(|nv| nv.as_ref())
    .collect();
  let auto_resolved_nvs: FxHashSet<PackageNv> = tree
    .root_packages
    .keys()
    .filter(|nv| !package_req_nvs.contains(nv.as_ref()))
    .map(|nv| (**nv).clone())
    .collect();

  let mut ctx = PeerResolutionCtx {
    tree,
    all_results: Vec::with_capacity(tree.nodes.len()),
    peers_cache: HashMap::with_hasher(FxBuildHasher),
    unmet_peer_diagnostics: IndexSet::new(),
    auto_resolved_nvs,
    partial_peer_node_ids: FxHashMap::default(),
  };

  let root_parent_pkgs = {
    let mut pkgs = ParentPackages::new();
    // Root-level visible packages include all root packages
    for (nv, &node_id) in &tree.root_packages {
      pkgs.pkgs.insert(nv.name.clone(), (nv.clone(), node_id));
    }
    pkgs
  };

  let mut ancestor_path = Vec::new();
  let mut ancestor_set = FxHashSet::default();
  for (_, &node_id) in &tree.root_packages {
    resolve_peers_of_node(
      node_id,
      &root_parent_pkgs,
      &mut ctx,
      &mut ancestor_path,
      &mut ancestor_set,
      &[],
    );
  }

  // Reconstruct all NpmPackageIds using v1-style cycle-aware recursion.
  // During the DFS, cycle back-edges produce placeholder NpmPackageIds.
  // This pass rebuilds all IDs with correct nested peer encoding.
  rebuild_npm_package_ids(&mut ctx.all_results, tree);

  // For each root package, find the best resolution from all_results.
  // A root package that also appears as a peer dep in a deeper context
  // may have a better resolution (with more peer deps) than the root-level one.
  // Use the root-level resolution (first entry) by default, and only
  // replace it if a deeper resolution has strictly MORE peer deps.
  let mut root_resolved = FxHashMap::with_capacity_and_hasher(tree.root_packages.len(), Default::default());
  for (_, &node_id) in &tree.root_packages {
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
  bubbling_peers: FxHashMap<StackString, NpmPackageId>,
  /// Ordered peer node IDs that should bubble to the parent
  /// (for post-DFS NpmPackageId reconstruction).
  bubbling_peer_node_ids: Vec<(StackString, DepTreeNodeId)>,
}

/// Recursively resolve peers for a node and its subtree.
///
/// Returns the resolved NpmPackageId plus any peer deps that should
/// propagate upward through ancestor identities.
///
/// Key ordering: resolves own peer deps FIRST, then recurses into children.
/// This ensures ancestors' peers are known before children are resolved,
/// matching v1's lazy reference behavior.
fn resolve_peers_of_node(
  node_id: DepTreeNodeId,
  parent_pkgs: &ParentPackages,
  ctx: &mut PeerResolutionCtx,
  ancestor_path: &mut Vec<DepTreeNodeId>,
  ancestor_set: &mut FxHashSet<DepTreeNodeId>,
  ancestor_nvs: &[Rc<PackageNv>],
) -> NodePeerResult {
  let is_root_level = ancestor_nvs.is_empty();
  let node = ctx.tree.get_node(node_id);
  let nv = node.nv.clone();

  // Check cache: same nv + same visible peer package set → same result.
  let cache_key = make_cache_key(node_id, parent_pkgs);
  if let Some(cached) = ctx.peers_cache.get(&cache_key) {
    let result = cached.clone();
    // Filter bubbling by child_pkg_names: peers that are regular dep
    // children of this node should not bubble to the caller. This matches
    // the filtering done in the non-cache path (lines ~589-599).
    let node_child_pkg_names: FxHashSet<StackString> = ctx
      .tree
      .get_node(node_id)
      .children
      .values()
      .map(|&cid| ctx.tree.get_node(cid).nv.name.clone())
      .collect();
    let bubbling = result
      .all_resolved_peers
      .iter()
      .filter(|(name, _)| !node_child_pkg_names.contains(name.as_str()))
      .filter(|(_, peer_id)| !ctx.auto_resolved_nvs.contains(&peer_id.nv))
      .map(|(k, v)| (k.clone(), v.clone()))
      .collect();
    let bubbling_node_ids = result
      .all_resolved_peer_node_ids
      .iter()
      .filter(|(name, _)| !node_child_pkg_names.contains(name.as_str()))
      .filter(|(_, nid)| {
        !ctx
          .auto_resolved_nvs
          .contains(&ctx.tree.get_node(*nid).nv)
      })
      .cloned()
      .collect();
    ctx.all_results.push((node_id, result));
    return NodePeerResult {
      pkg_id: ctx.all_results.last().unwrap().1.pkg_id.clone(),
      bubbling_peers: bubbling,
      bubbling_peer_node_ids: bubbling_node_ids,
    };
  }

  // Detect cycles: if this node is already in the ancestor path,
  // return a placeholder ID and bubble the ancestor's own peers.
  if ancestor_set.contains(&node_id) {
    let partial = ctx
      .partial_peer_node_ids
      .get(&node_id)
      .cloned()
      .unwrap_or_default();
    return NodePeerResult {
      pkg_id: NpmPackageId {
        nv: (*nv).clone(),
        peer_dependencies: Default::default(),
      },
      bubbling_peers: FxHashMap::default(),
      bubbling_peer_node_ids: partial,
    };
  }

  ancestor_path.push(node_id);
  ancestor_set.insert(node_id);

  // Build parent_pkgs for children: current parent_pkgs + this node's children
  let child_parent_pkgs = parent_pkgs.extended_with(ctx.tree, node_id);

  // Collect the names of this node's direct children (for filtering later)
  let children: Vec<_> = node
    .children
    .iter()
    .map(|(k, v)| (k.clone(), *v))
    .collect();
  let child_pkg_names: FxHashSet<StackString> = children
    .iter()
    .map(|(_, child_id)| ctx.tree.get_node(*child_id).nv.name.clone())
    .collect();

  let mut all_resolved_peers: FxHashMap<StackString, NpmPackageId> =
    FxHashMap::default();
  let mut all_resolved_peer_node_ids: Vec<(StackString, DepTreeNodeId)> =
    Vec::new();
  // Track names already in all_resolved_peer_node_ids for O(1) dedup checks
  let mut peer_node_id_names: FxHashSet<StackString> = FxHashSet::default();
  let mut dep_node_ids: FxHashMap<StackString, DepTreeNodeId> = FxHashMap::default();
  let dep_capacity = node.children.len() + node.peer_dep_specifiers.len();
  let mut dependencies = FxHashMap::with_capacity_and_hasher(dep_capacity, Default::default());

  let optional_peer_dep_specifiers: FxHashSet<StackString> = node.optional_peer_dep_specifiers.iter().cloned().collect();
  let deps = node.deps.clone();

  // ── Phase 1: resolve OWN peer deps first ──
  // Collect bubbling peer node IDs separately so that own direct peers
  // are added to all_resolved_peer_node_ids first (matching v1's BFS
  // ordering where own peers come before bubbled peers from children).
  let mut deferred_bubbling_node_ids: Vec<(StackString, DepTreeNodeId)> =
    Vec::new();
  for dep in deps.iter() {
    if !matches!(
      dep.kind,
      NpmDependencyEntryKind::Peer | NpmDependencyEntryKind::OptionalPeer
    ) {
      continue;
    }

    let specifier = &dep.bare_specifier;

    // Issue C fix: check if any of this node's own children (regular deps)
    // already resolve to a package matching the peer dep name.
    // This handles aliased deps like "package-peer2" → package-peer@2.0.0
    // satisfying a peer dep on "package-peer".
    let already_resolved_by_child = children
      .iter()
      .find(|(_, child_id)| ctx.tree.get_node(*child_id).nv.name == dep.name);

    if let Some((_child_spec, child_id)) = already_resolved_by_child {
      let child_id = *child_id;
      let mut peer_ancestor_nvs = ancestor_nvs.to_vec();
      peer_ancestor_nvs.push(nv.clone());
      let peer_result = resolve_peers_of_node(
        child_id,
        &child_parent_pkgs,
        ctx,
        ancestor_path,
        ancestor_set,
        &peer_ancestor_nvs,
      );
      dependencies.insert(specifier.clone(), peer_result.pkg_id.clone());
      dep_node_ids.insert(specifier.clone(), child_id);
      all_resolved_peers
        .insert(dep.name.clone(), peer_result.pkg_id);
      if peer_node_id_names.insert(dep.name.clone()) {
        all_resolved_peer_node_ids.push((dep.name.clone(), child_id));
      }
      for (peer_name, peer_id) in peer_result.bubbling_peers {
        all_resolved_peers
          .entry(peer_name.clone())
          .or_insert(peer_id);
      }
      for (name, nid) in peer_result.bubbling_peer_node_ids {
        deferred_bubbling_node_ids.push((name, nid));
      }
    } else if let Some((peer_nv, peer_node_id)) =
      parent_pkgs.find(&dep.name, &dep.version_req).filter(|(nv, _)| {
        // Skip optional peers resolved only via auto-resolution. In v1,
        // auto-resolved peers are local children of the requesting node,
        // not globally visible, so optional peers in unrelated packages
        // don't pick them up.
        dep.kind != NpmDependencyEntryKind::OptionalPeer
          || !ctx.auto_resolved_nvs.contains(nv)
      })
    {
      // Found a matching peer in the parent context.
      if !is_root_level
        && dep.version_req.tag().is_none()
        && !dep.version_req.matches(&peer_nv.version)
      {
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
        ancestor_set,
        &peer_ancestor_nvs,
      );
      dependencies.insert(specifier.clone(), peer_result.pkg_id.clone());
      dep_node_ids.insert(specifier.clone(), peer_node_id);
      all_resolved_peers
        .insert(dep.name.clone(), peer_result.pkg_id);
      if peer_node_id_names.insert(dep.name.clone()) {
        all_resolved_peer_node_ids.push((dep.name.clone(), peer_node_id));
      }
      for (peer_name, peer_id) in peer_result.bubbling_peers {
        all_resolved_peers
          .entry(peer_name.clone())
          .or_insert(peer_id);
      }
      for (name, nid) in peer_result.bubbling_peer_node_ids {
        deferred_bubbling_node_ids.push((name, nid));
      }
    } else if !is_root_level
      && !optional_peer_dep_specifiers.contains(specifier)
    {
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
  }
  // Now add the deferred bubbling peer node IDs (after all own direct peers).
  for (name, nid) in deferred_bubbling_node_ids {
    if peer_node_id_names.insert(name.clone()) {
      all_resolved_peer_node_ids.push((name, nid));
    }
  }

  // Store partial peer node IDs for cycle back-edges.
  // At this point, own peers are resolved but children haven't been processed.
  ctx
    .partial_peer_node_ids
    .insert(node_id, all_resolved_peer_node_ids.clone());

  // Propagate newly resolved peers to all ancestors' partials.
  // This mirrors v1's `add_peer_deps_to_path`: when a peer dep is resolved,
  // ALL ancestors in the current DFS path get that peer added to their
  // partial state. This ensures that cycle back-edges to those ancestors
  // will include the newly discovered peer.
  propagate_peers_to_ancestors(
    &all_resolved_peer_node_ids,
    ancestor_path,
    ctx,
  );

  // ── Phase 2: recurse into regular children ──
  for (specifier, child_id) in &children {
    let mut child_ancestor_nvs = ancestor_nvs.to_vec();
    child_ancestor_nvs.push(nv.clone());
    let child_result = resolve_peers_of_node(
      *child_id,
      &child_parent_pkgs,
      ctx,
      ancestor_path,
      ancestor_set,
      &child_ancestor_nvs,
    );
    dependencies.insert(specifier.clone(), child_result.pkg_id);
    dep_node_ids.insert(specifier.clone(), *child_id);

    let mut new_bubbled = Vec::new();
    for (peer_name, peer_id) in child_result.bubbling_peers {
      all_resolved_peers
        .entry(peer_name.clone())
        .or_insert(peer_id);
    }
    for (name, nid) in child_result.bubbling_peer_node_ids {
      if peer_node_id_names.insert(name.clone()) {
        all_resolved_peer_node_ids.push((name.clone(), nid));
        new_bubbled.push((name, nid));
      }
    }

    // After each child, update this node's partial and propagate to ancestors.
    // This ensures that when later children's descendants encounter cycle
    // back-edges to this node or its ancestors, they see the updated peer set
    // (including peers discovered from earlier sibling branches).
    if !new_bubbled.is_empty() {
      ctx
        .partial_peer_node_ids
        .insert(node_id, all_resolved_peer_node_ids.clone());
      propagate_peers_to_ancestors(&new_bubbled, ancestor_path, ctx);
    }
  }

  // Remove self-references
  all_resolved_peers.remove(nv.name.as_str());
  all_resolved_peer_node_ids.retain(|(n, _)| *n != nv.name);

  // Ensure all_resolved_peers has entries for all peer node IDs.
  // Cycle returns populate bubbling_peer_node_ids but not bubbling_peers,
  // so there may be entries in all_resolved_peer_node_ids without
  // corresponding entries in all_resolved_peers. Fill them with bare IDs
  // (the post-DFS rebuild will compute the correct nested structure).
  for (name, nid) in &all_resolved_peer_node_ids {
    all_resolved_peers.entry(name.clone()).or_insert_with(|| {
      let peer_nv = (*ctx.tree.get_node(*nid).nv).clone();
      NpmPackageId {
        nv: peer_nv,
        peer_dependencies: Default::default(),
      }
    });
  }

  // Build NpmPackageId (placeholder — will be rebuilt by post-DFS pass)
  let mut peer_dependencies = NpmPackageIdPeerDependencies::with_capacity(
    all_resolved_peer_node_ids.len(),
  );
  let mut seen_peer_ids = FxHashSet::default();
  for (_name, peer_nid) in &all_resolved_peer_node_ids {
    let peer_name = &ctx.tree.get_node(*peer_nid).nv.name;
    if let Some(peer_id) = all_resolved_peers.get(peer_name.as_str()) {
      if seen_peer_ids.insert(peer_id.clone()) {
        peer_dependencies.push(peer_id.clone());
      }
    }
  }

  let pkg_id = NpmPackageId {
    nv: (*nv).clone(),
    peer_dependencies,
  };

  // Compute bubbling peers
  let bubbling_peers: FxHashMap<StackString, NpmPackageId> = all_resolved_peers
    .iter()
    .filter(|(name, _)| !child_pkg_names.contains(name.as_str()))
    .filter(|(_, peer_id)| !ctx.auto_resolved_nvs.contains(&peer_id.nv))
    .map(|(k, v)| (k.clone(), v.clone()))
    .collect();
  let bubbling_peer_node_ids: Vec<(StackString, DepTreeNodeId)> =
    all_resolved_peer_node_ids
      .iter()
      .filter(|(name, _)| !child_pkg_names.contains(name.as_str()))
      .filter(|(_, nid)| {
        !ctx
          .auto_resolved_nvs
          .contains(&ctx.tree.get_node(*nid).nv)
      })
      .cloned()
      .collect();

  let version_info = &node.version_info;
  let optional_dependencies: FxHashSet<StackString> = version_info
    .optional_dependencies
    .keys()
    .cloned()
    .collect();

  let result = ResolvedNodePeers {
    pkg_id: pkg_id.clone(),
    dependencies,
    dep_node_ids,
    optional_dependencies,
    optional_peer_dependencies: optional_peer_dep_specifiers,
    all_resolved_peers,
    all_resolved_peer_node_ids,
  };

  ctx.all_results.push((node_id, result.clone()));
  ctx.peers_cache.insert(cache_key, result);

  ctx.partial_peer_node_ids.remove(&node_id);
  ancestor_path.pop();
  ancestor_set.remove(&node_id);

  NodePeerResult {
    pkg_id,
    bubbling_peers,
    bubbling_peer_node_ids,
  }
}

/// Propagate resolved peers to ancestors' `partial_peer_node_ids`.
///
/// Mirrors v1's `add_peer_deps_to_path`: when a peer dep is resolved at node N,
/// ancestors in the DFS path between N and the resolution point get that peer
/// added to their partial state. Propagation of each peer STOPS at the ancestor
/// that has the peer as a direct child (the resolution point), matching v1's
/// path construction which only spans from the requesting node to the ancestor
/// where the peer was found.
fn propagate_peers_to_ancestors(
  new_peers: &[(StackString, DepTreeNodeId)],
  ancestor_path: &[DepTreeNodeId],
  ctx: &mut PeerResolutionCtx,
) {
  if new_peers.is_empty() {
    return;
  }

  // Track which peers are still being propagated upward.
  // Each peer stops propagating when we reach the ancestor that has it
  // as a direct child (the resolution point).
  let mut active: Vec<bool> = vec![true; new_peers.len()];

  // The last element in ancestor_path is the current node itself (just pushed).
  // Propagate to ancestors ABOVE the current node.
  for &ancestor_id in ancestor_path.iter().rev().skip(1) {
    // Check if all peers have been stopped
    if active.iter().all(|&a| !a) {
      break;
    }

    let ancestor_nv_name = ctx.tree.get_node(ancestor_id).nv.name.clone();
    let ancestor_child_names: FxHashSet<StackString> = ctx
      .tree
      .get_node(ancestor_id)
      .children
      .values()
      .map(|&cid| ctx.tree.get_node(cid).nv.name.clone())
      .collect();

    if let Some(partial) = ctx.partial_peer_node_ids.get_mut(&ancestor_id) {
      for (i, (name, nid)) in new_peers.iter().enumerate() {
        if !active[i] {
          continue;
        }
        // Stop propagating self-references
        if name.as_str() == ancestor_nv_name.as_str() {
          active[i] = false;
          continue;
        }
        // Stop propagating at the resolution point (ancestor has peer as child)
        if ancestor_child_names.contains(name.as_str()) {
          active[i] = false;
          continue;
        }
        if !partial.iter().any(|(n, _)| n == name) {
          partial.push((name.clone(), *nid));
        }
      }
    } else {
      // Even without a partial entry, check if this is a resolution point
      for (i, (name, _)) in new_peers.iter().enumerate() {
        if !active[i] {
          continue;
        }
        if name.as_str() == ancestor_nv_name.as_str() {
          active[i] = false;
        } else if ancestor_child_names.contains(name.as_str()) {
          active[i] = false;
        }
      }
    }
  }
}

/// Build a cache key for peer resolution memoization.
///
/// Includes ALL parent packages in the key (not just the node's own peer deps)
/// because descendant nodes may have peer deps that resolve differently
/// depending on the full parent context. Without this, a node visited from
/// two different parent contexts with different visible packages would get
/// a cache hit when it shouldn't.
fn make_cache_key(
  node_id: DepTreeNodeId,
  parent_pkgs: &ParentPackages,
) -> (DepTreeNodeId, Vec<DepTreeNodeId>) {
  let mut parent_node_ids: Vec<DepTreeNodeId> = parent_pkgs
    .pkgs
    .values()
    .map(|(_, nid)| *nid)
    .collect();
  parent_node_ids.sort();
  (node_id, parent_node_ids)
}

// ======================================================================
// Post-DFS NpmPackageId reconstruction
// ======================================================================

/// Reconstruct all NpmPackageIds using v1-style cycle-aware recursion.
///
/// During the DFS, peers only propagate through visited paths. In v1,
/// peers propagate through ALL paths a node appears on (including
/// cross-sibling paths). This pass:
/// 1. Propagates peers from deps to parents (matching v1's behavior)
/// 2. Rebuilds NpmPackageIds from the updated peer sets
/// 3. Updates dependency references
fn rebuild_npm_package_ids(
  all_results: &mut Vec<(DepTreeNodeId, ResolvedNodePeers)>,
  tree: &DepTree,
) {
  // Build map: DepTreeNodeId → peer node IDs (use entry with most peers).
  let mut peer_map: FxHashMap<DepTreeNodeId, Vec<(StackString, DepTreeNodeId)>> =
    FxHashMap::default();
  for (node_id, resolved) in all_results.iter() {
    let entry = peer_map.entry(*node_id).or_default();
    if resolved.all_resolved_peer_node_ids.len() > entry.len() {
      *entry = resolved.all_resolved_peer_node_ids.clone();
    }
  }

  // Build canonical pkg_id for each DepTreeNodeId (used for cycle back-edges).
  let mut node_to_pkg_id: FxHashMap<DepTreeNodeId, NpmPackageId> =
    FxHashMap::with_capacity_and_hasher(peer_map.len(), Default::default());
  for (&node_id, peer_node_ids) in &peer_map {
    let nv = (*tree.get_node(node_id).nv).clone();
    let mut seen = FxHashSet::from_iter([nv.clone()]);
    let pkg_id =
      build_npm_pkg_id(peer_node_ids, &nv, tree, &peer_map, &mut seen);
    node_to_pkg_id.insert(node_id, pkg_id);
  }

  // Phase 1: Rebuild each entry's pkg_id and collect old → new mapping.
  let mut id_mapping: FxHashMap<NpmPackageId, NpmPackageId> = FxHashMap::default();
  for idx in 0..all_results.len() {
    let (node_id, _) = all_results[idx];
    let nv = (*tree.get_node(node_id).nv).clone();

    let mut seen = FxHashSet::from_iter([nv.clone()]);
    let new_pkg_id = build_npm_pkg_id(
      &all_results[idx].1.all_resolved_peer_node_ids,
      &nv,
      tree,
      &peer_map,
      &mut seen,
    );

    let old_pkg_id = &all_results[idx].1.pkg_id;
    if *old_pkg_id != new_pkg_id {
      id_mapping.insert(old_pkg_id.clone(), new_pkg_id.clone());
    }
    all_results[idx].1.pkg_id = new_pkg_id;
  }

  // Phase 2: Update dependency references.
  // Simple (non-recursive) id_mapping lookup for DFS placeholders,
  // then fall back to node_to_pkg_id for cycle back-edge bare references.
  for idx in 0..all_results.len() {
    let dep_updates: Vec<(StackString, NpmPackageId)> = all_results[idx]
      .1
      .dependencies
      .iter()
      .filter_map(|(spec, dep_id)| {
        if let Some(new_id) = id_mapping.get(dep_id) {
          return Some((spec.clone(), new_id.clone()));
        }
        // Cycle back-edge: bare ID (no peers) where canonical has peers.
        if dep_id.peer_dependencies.iter().next().is_none() {
          if let Some(&dep_nid) = all_results[idx].1.dep_node_ids.get(spec) {
            if let Some(canonical) = node_to_pkg_id.get(&dep_nid) {
              if canonical.peer_dependencies.iter().next().is_some() {
                return Some((spec.clone(), canonical.clone()));
              }
            }
          }
        }
        None
      })
      .collect();
    for (spec, new_id) in dep_updates {
      all_results[idx].1.dependencies.insert(spec, new_id);
    }
  }
}

/// Build an NpmPackageId from peer node IDs using cycle-aware recursion.
///
/// Mirrors v1's `get_npm_pkg_id_from_resolved_id_with_seen`:
/// - `seen` tracks PackageNv values currently being serialized
/// - When a cycle is detected (peer's nv already in `seen`), use bare nv
/// - This produces correctly nested peer encodings
fn build_npm_pkg_id(
  peer_node_ids: &[(StackString, DepTreeNodeId)],
  nv: &PackageNv,
  tree: &DepTree,
  peer_map: &FxHashMap<DepTreeNodeId, Vec<(StackString, DepTreeNodeId)>>,
  seen: &mut FxHashSet<PackageNv>,
) -> NpmPackageId {
  let mut peer_dependencies =
    NpmPackageIdPeerDependencies::with_capacity(peer_node_ids.len());
  let mut seen_peer_ids = FxHashSet::default();

  for (_name, peer_node_id) in peer_node_ids {
    let peer_nv = (*tree.get_node(*peer_node_id).nv).clone();
    if seen.insert(peer_nv.clone()) {
      let child_peer_node_ids =
        peer_map.get(peer_node_id).cloned().unwrap_or_default();
      let child_peer = build_npm_pkg_id(
        &child_peer_node_ids,
        &peer_nv,
        tree,
        peer_map,
        seen,
      );
      seen.remove(&peer_nv);
      if seen_peer_ids.insert(child_peer.clone()) {
        peer_dependencies.push(child_peer);
      }
    } else {
      // Cycle — use bare nv
      let bare = NpmPackageId {
        nv: peer_nv,
        peer_dependencies: Default::default(),
      };
      if seen_peer_ids.insert(bare.clone()) {
        peer_dependencies.push(bare);
      }
    }
  }

  NpmPackageId {
    nv: nv.clone(),
    peer_dependencies,
  }
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
  let superset_dep_values: FxHashSet<&NpmPackageId> =
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
) -> FxHashMap<NpmPackageId, NpmPackageId> {
  // Group by PackageNv → unique NpmPackageId → representative ResolvedNodePeers
  let mut by_nv: FxHashMap<&PackageNv, FxHashMap<&NpmPackageId, &ResolvedNodePeers>> =
    FxHashMap::default();
  for (_, resolved) in all_resolved {
    by_nv
      .entry(&resolved.pkg_id.nv)
      .or_default()
      .entry(&resolved.pkg_id)
      .or_insert(resolved);
  }

  let mut merge_map = FxHashMap::default();

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
  merge_map: &FxHashMap<NpmPackageId, NpmPackageId>,
  cache: &mut FxHashMap<NpmPackageId, NpmPackageId>,
) -> NpmPackageId {
  // Check memoization cache first
  if let Some(cached) = cache.get(id) {
    return cached.clone();
  }

  // Check if this exact ID is in the merge map
  if let Some(new_id) = merge_map.get(id) {
    cache.insert(id.clone(), new_id.clone());
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
    let new_peer = apply_merge_to_id(peer, merge_map, cache);
    if new_peer != *peer {
      changed = true;
    }
    new_peers.push(new_peer);
  }

  let result = if changed {
    let result = NpmPackageId {
      nv: id.nv.clone(),
      peer_dependencies: new_peers,
    };
    // Check if the resulting ID itself should be merged
    merge_map.get(&result).cloned().unwrap_or(result)
  } else {
    id.clone()
  };

  cache.insert(id.clone(), result.clone());
  result
}

/// Apply the merge map to all NpmPackageIds in all_resolved entries.
fn apply_merge_map(
  all_resolved: &mut [(DepTreeNodeId, ResolvedNodePeers)],
  merge_map: &FxHashMap<NpmPackageId, NpmPackageId>,
) {
  let mut cache = FxHashMap::default();
  for (_, resolved) in all_resolved.iter_mut() {
    resolved.pkg_id = apply_merge_to_id(&resolved.pkg_id, merge_map, &mut cache);
    for dep_id in resolved.dependencies.values_mut() {
      *dep_id = apply_merge_to_id(dep_id, merge_map, &mut cache);
    }
    for peer_id in resolved.all_resolved_peers.values_mut() {
      *peer_id = apply_merge_to_id(peer_id, merge_map, &mut cache);
    }
  }
}

/// Apply the merge map to all NpmPackageIds in a HashMap of entries.
fn apply_merge_map_entries(
  entries: &mut FxHashMap<DepTreeNodeId, ResolvedNodePeers>,
  merge_map: &FxHashMap<NpmPackageId, NpmPackageId>,
) {
  let mut cache = FxHashMap::default();
  for resolved in entries.values_mut() {
    resolved.pkg_id = apply_merge_to_id(&resolved.pkg_id, merge_map, &mut cache);
    for dep_id in resolved.dependencies.values_mut() {
      *dep_id = apply_merge_to_id(dep_id, merge_map, &mut cache);
    }
    for peer_id in resolved.all_resolved_peers.values_mut() {
      *peer_id = apply_merge_to_id(peer_id, merge_map, &mut cache);
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
    FxHashMap::<NpmPackageId, (DepTreeNodeId, &ResolvedNodePeers)>::with_capacity_and_hasher(
      peer_result.all_resolved.len(),
      Default::default(),
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
  let package_req_nvs: FxHashSet<&PackageNv> =
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
  let mut traversed_ids = FxHashSet::with_capacity_and_hasher(peer_result.all_resolved.len(), Default::default());
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
        dependencies: resolved.dependencies.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        optional_dependencies: resolved.optional_dependencies.iter().cloned().collect(),
        optional_peer_dependencies: resolved
          .optional_peer_dependencies
          .iter().cloned().collect(),
        extra: Some(NpmPackageExtraInfo {
          bin: version_info.bin.clone(),
          scripts: version_info.scripts.clone(),
          deprecated: version_info.deprecated.clone(),
        }),
        is_deprecated: version_info.deprecated.is_some(),
        has_bin: version_info.bin.is_some(),
        has_scripts: version_info.has_install_script
          || version_info.scripts.contains_key("preinstall")
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
