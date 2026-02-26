// Copyright 2018-2026 the Deno authors. MIT license.

//! Phase 1 of the two-phase npm dependency resolution.
//!
//! Builds a dependency tree by resolving package versions via BFS
//! without resolving peer dependencies. Peer dependencies are recorded
//! as metadata on each node for Phase 2 to resolve on the frozen tree.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use deno_semver::StackString;
use deno_semver::Version;
use deno_semver::VersionReq;
use deno_semver::package::PackageName;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReq;
use log::debug;
use rustc_hash::FxHashMap;

use super::common::NpmPackageVersionResolver;
use super::common::NpmVersionResolver;
use super::graph::NpmResolutionError;
use super::graph::Reporter;
use super::overrides::NpmOverrides;
use super::snapshot::NpmResolutionSnapshot;
use crate::NpmPackageId;
use crate::registry::NpmDependencyEntry;
use crate::registry::NpmDependencyEntryError;
use crate::registry::NpmDependencyEntryKind;
use crate::registry::NpmPackageInfo;
use crate::registry::NpmPackageVersionInfo;
use crate::registry::NpmRegistryApi;
use crate::registry::NpmRegistryPackageInfoLoadError;

/// Index into `DepTree::nodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct DepTreeNodeId(u32);

/// A node in the Phase 1 dependency tree.
///
/// Each node represents a resolved package version. Regular (non-peer)
/// dependencies are stored as children. Peer dependency information is
/// recorded as metadata for Phase 2.
#[derive(Debug)]
pub struct DepTreeNode {
  pub nv: Rc<PackageNv>,
  /// Regular (non-peer) children: bare specifier → child node id
  pub children: BTreeMap<StackString, DepTreeNodeId>,
  /// All dependency entries from the manifest (peers included as metadata)
  pub deps: Rc<Vec<NpmDependencyEntry>>,
  /// Which bare specifiers are peer deps
  pub peer_dep_specifiers: HashSet<StackString>,
  /// Which bare specifiers are optional peer deps
  pub optional_peer_dep_specifiers: HashSet<StackString>,
  /// Override context at this tree position
  pub active_overrides: Rc<NpmOverrides>,
  /// Cached version info for snapshot building (avoids re-fetch)
  pub version_info: Arc<NpmPackageVersionInfo>,
  /// Whether this node has no peer deps in its subtree (optimization)
  pub no_peers: bool,
}

/// The Phase 1 dependency tree.
///
/// Contains all resolved package versions with regular dependencies
/// as tree edges. Peer dependencies are recorded as metadata but
/// not resolved into child edges — that's Phase 2's job.
pub struct DepTree {
  pub package_reqs: HashMap<PackageReq, Rc<PackageNv>>,
  pub root_packages: BTreeMap<Rc<PackageNv>, DepTreeNodeId>,
  pub nodes: Vec<DepTreeNode>,
  /// Set of all package names that appear as peer deps anywhere
  pub all_peer_dep_names: HashSet<StackString>,
  /// Tracks all resolved versions per package name (for dedup & version reuse)
  pub package_name_versions: HashMap<StackString, HashSet<Version>>,
  /// Index from PackageNv → DepTreeNodeId for O(1) node lookup by nv.
  nv_to_node: FxHashMap<PackageNv, DepTreeNodeId>,
}

impl DepTree {
  pub fn new() -> Self {
    Self {
      package_reqs: HashMap::new(),
      root_packages: BTreeMap::new(),
      nodes: Vec::new(),
      all_peer_dep_names: HashSet::new(),
      package_name_versions: HashMap::new(),
      nv_to_node: FxHashMap::default(),
    }
  }

  /// Reconstruct a `DepTree` from an existing `NpmResolutionSnapshot`.
  ///
  /// Creates a node for each package, populating only regular (non-peer)
  /// children from `pkg.dependencies`. Records `optional_peer_dependencies`
  /// as peer dep metadata.
  pub fn from_snapshot(
    snapshot: NpmResolutionSnapshot,
    version_resolver: &NpmVersionResolver,
    api_cached_infos: &HashMap<PackageName, Arc<NpmPackageInfo>>,
  ) -> Self {
    let mut tree = Self::new();
    // Map from NpmPackageId → DepTreeNodeId for already-created nodes
    let mut created_nodes: HashMap<NpmPackageId, DepTreeNodeId> =
      HashMap::with_capacity(snapshot.packages.len());

    // First pass: create all nodes
    for (pkg_id, pkg) in &snapshot.packages {
      let nv = Rc::new(pkg_id.nv.clone());

      // Get version info if available from cached API data
      let version_info = api_cached_infos
        .get(&nv.name)
        .and_then(|info| {
          info
            .version_info(&nv, &version_resolver.link_packages)
            .ok()
        })
        .map(|vi| Arc::new(vi.clone()))
        .unwrap_or_else(|| {
          // Create a minimal version info from snapshot data
          Arc::new(NpmPackageVersionInfo {
            version: nv.version.clone(),
            dist: pkg.dist.clone(),
            ..Default::default()
          })
        });

      // Compute deps from version info
      let mut deps = version_info
        .dependencies_as_entries(&nv.name)
        .unwrap_or_default();

      // Remove optional peer deps that weren't resolved in the snapshot.
      // Once an optional peer dep is unresolved, it should stay unresolved
      // in subsequent re-resolutions from this snapshot — preserving the
      // user's intent of not installing the optional peer.
      deps.retain(|dep| {
        if dep.kind == NpmDependencyEntryKind::OptionalPeer
          && !pkg.dependencies.contains_key(&dep.bare_specifier)
        {
          return false;
        }
        true
      });
      let deps = Rc::new(deps);

      // Extract peer dep specifiers
      let mut peer_dep_specifiers = HashSet::new();
      let mut optional_peer_dep_specifiers = HashSet::new();
      for dep in deps.iter() {
        match dep.kind {
          NpmDependencyEntryKind::Peer => {
            peer_dep_specifiers.insert(dep.bare_specifier.clone());
            tree.all_peer_dep_names.insert(dep.name.as_str().into());
          }
          NpmDependencyEntryKind::OptionalPeer => {
            peer_dep_specifiers.insert(dep.bare_specifier.clone());
            optional_peer_dep_specifiers
              .insert(dep.bare_specifier.clone());
            tree.all_peer_dep_names.insert(dep.name.as_str().into());
          }
          NpmDependencyEntryKind::Dep => {}
        }
      }

      // Also record optional_peer_dependencies from the snapshot
      // (only those that were actually resolved)
      for key in &pkg.optional_peer_dependencies {
        if pkg.dependencies.contains_key(key) {
          peer_dep_specifiers.insert(key.clone());
          optional_peer_dep_specifiers.insert(key.clone());
        }
      }

      tree
        .package_name_versions
        .entry(nv.name.clone())
        .or_default()
        .insert(nv.version.clone());

      let no_peers = peer_dep_specifiers.is_empty();
      let node_id = DepTreeNodeId(tree.nodes.len() as u32);
      tree.nv_to_node.insert((*nv).clone(), node_id);
      tree.nodes.push(DepTreeNode {
        nv,
        children: BTreeMap::new(),
        deps,
        peer_dep_specifiers,
        optional_peer_dep_specifiers,
        active_overrides: Rc::new(NpmOverrides::default()),
        version_info,
        no_peers,
      });
      created_nodes.insert(pkg_id.clone(), node_id);
    }

    // Second pass: wire up regular (non-peer) children
    for (pkg_id, pkg) in &snapshot.packages {
      let parent_id = *created_nodes.get(pkg_id).unwrap();
      let parent_peers = &tree.nodes[parent_id.0 as usize]
        .peer_dep_specifiers
        .clone();

      for (specifier, child_pkg_id) in &pkg.dependencies {
        // Skip peer dep children — Phase 2 handles those
        if parent_peers.contains(specifier) {
          continue;
        }
        if let Some(&child_node_id) = created_nodes.get(child_pkg_id) {
          // Guard against packages that incorrectly list themselves as a
          // dependency (see dep_tree_from_snapshot_dep_on_self test).
          if child_node_id != parent_id {
            tree.nodes[parent_id.0 as usize]
              .children
              .insert(specifier.clone(), child_node_id);
          }
        }
      }
    }

    // Set up root packages and package reqs
    for (nv, pkg_id) in &snapshot.root_packages {
      if let Some(&node_id) = created_nodes.get(pkg_id) {
        tree
          .root_packages
          .insert(Rc::new(nv.clone()), node_id);
      }
    }
    for (req, nv) in &snapshot.package_reqs {
      tree
        .package_reqs
        .insert(req.clone(), Rc::new(nv.clone()));
    }

    tree
  }

  pub fn get_node(&self, id: DepTreeNodeId) -> &DepTreeNode {
    &self.nodes[id.0 as usize]
  }

  fn create_node(
    &mut self,
    nv: Rc<PackageNv>,
    deps: Rc<Vec<NpmDependencyEntry>>,
    version_info: Arc<NpmPackageVersionInfo>,
    active_overrides: Rc<NpmOverrides>,
  ) -> DepTreeNodeId {
    let mut peer_dep_specifiers = HashSet::new();
    let mut optional_peer_dep_specifiers = HashSet::new();
    let mut has_peers = false;

    for dep in deps.iter() {
      match dep.kind {
        NpmDependencyEntryKind::Peer => {
          peer_dep_specifiers.insert(dep.bare_specifier.clone());
          self.all_peer_dep_names.insert(dep.name.as_str().into());
          has_peers = true;
        }
        NpmDependencyEntryKind::OptionalPeer => {
          peer_dep_specifiers.insert(dep.bare_specifier.clone());
          optional_peer_dep_specifiers
            .insert(dep.bare_specifier.clone());
          self.all_peer_dep_names.insert(dep.name.as_str().into());
          has_peers = true;
        }
        NpmDependencyEntryKind::Dep => {}
      }
    }

    self
      .package_name_versions
      .entry(nv.name.clone())
      .or_default()
      .insert(nv.version.clone());

    let id = DepTreeNodeId(self.nodes.len() as u32);
    self.nv_to_node.insert((*nv).clone(), id);
    self.nodes.push(DepTreeNode {
      nv,
      children: BTreeMap::new(),
      deps,
      peer_dep_specifiers,
      optional_peer_dep_specifiers,
      active_overrides,
      version_info,
      no_peers: !has_peers,
    });
    id
  }

  /// Find an existing node for the given nv (without peer deps).
  fn find_node_for_nv(&self, nv: &PackageNv) -> Option<DepTreeNodeId> {
    self.nv_to_node.get(nv).copied()
  }
}

/// Mutable state for the dep tree builder, wrapped in `RefCell` to allow
/// concurrent async resolution without `&mut self`.
struct DepTreeState {
  tree: DepTree,
  dep_entry_cache: DepEntryCache,
  pending: VecDeque<PendingNode>,
}

/// Manages building the Phase 1 dependency tree.
///
/// Uses `RefCell` for interior mutability so that concurrent async subtree
/// resolution can share `&self`. The critical invariant: `borrow_mut()` is
/// never held across an `.await` point.
pub struct DepTreeBuilder<'a, TNpmRegistryApi: NpmRegistryApi> {
  state: RefCell<DepTreeState>,
  api: &'a TNpmRegistryApi,
  version_resolver: &'a NpmVersionResolver,
  reporter: Option<&'a dyn Reporter>,
  should_dedup: bool,
  initial_overrides: Rc<NpmOverrides>,
}

/// A node waiting to have its dependencies resolved.
struct PendingNode {
  node_id: DepTreeNodeId,
  /// Ancestor nvs for cycle detection
  ancestors: Vec<Rc<PackageNv>>,
  /// Override context for this node's position in the tree.
  /// Computed via `NpmOverrides::for_child()` as we descend.
  active_overrides: Rc<NpmOverrides>,
}

/// Cache for parsed dependency entries keyed by package nv.
#[derive(Default)]
struct DepEntryCache(HashMap<Rc<PackageNv>, Rc<Vec<NpmDependencyEntry>>>);

impl DepEntryCache {
  pub fn store(
    &mut self,
    nv: Rc<PackageNv>,
    version_info: &NpmPackageVersionInfo,
  ) -> Result<Rc<Vec<NpmDependencyEntry>>, Box<NpmDependencyEntryError>> {
    debug_assert_eq!(nv.version, version_info.version);
    let mut deps = version_info.dependencies_as_entries(&nv.name)?;
    deps.sort();
    let deps = Rc::new(deps);
    self.0.insert(nv, deps.clone());
    Ok(deps)
  }

  pub fn get(&self, id: &PackageNv) -> Option<&Rc<Vec<NpmDependencyEntry>>> {
    self.0.get(id)
  }
}

impl<'a, TNpmRegistryApi: NpmRegistryApi> DepTreeBuilder<'a, TNpmRegistryApi> {
  pub fn new(
    tree: DepTree,
    api: &'a TNpmRegistryApi,
    version_resolver: &'a NpmVersionResolver,
    reporter: Option<&'a dyn Reporter>,
    should_dedup: bool,
  ) -> Self {
    let initial_overrides = Rc::new((*version_resolver.overrides).clone());
    Self {
      state: RefCell::new(DepTreeState {
        tree,
        dep_entry_cache: DepEntryCache::default(),
        pending: VecDeque::new(),
      }),
      api,
      version_resolver,
      reporter,
      should_dedup,
      initial_overrides,
    }
  }

  /// Add a top-level package requirement.
  pub fn add_package_req(
    &self,
    package_req: &PackageReq,
    package_info: &NpmPackageInfo,
  ) -> Result<Rc<PackageNv>, NpmResolutionError> {
    let mut state = self.state.borrow_mut();

    // Already resolved?
    if let Some(nv) = state.tree.package_reqs.get(package_req) {
      return Ok(nv.clone());
    }

    let version_resolver =
      self.version_resolver.get_for_package(package_info);

    // Check overrides for root-level packages
    let overrides = self.initial_overrides.clone();
    let req_version_req =
      match overrides.get_override_for(&package_req.name, None) {
        Some(req) => req,
        None => {
          let natural_version = version_resolver
            .resolve_best_package_version_info(
              &package_req.version_req,
              state
                .tree
                .package_name_versions
                .entry(version_resolver.info().name.clone())
                .or_default()
                .iter(),
            )
            .ok()
            .map(|info| info.version.clone());
          match natural_version
            .as_ref()
            .and_then(|v| overrides.get_override_for(&package_req.name, Some(v)))
          {
            Some(req) => req,
            None => &package_req.version_req,
          }
        }
      };

    // Check for existing root that satisfies
    let existing_root = state
      .tree
      .root_packages
      .iter()
      .find(|(nv, _)| {
        package_req.name == nv.name
          && version_resolver
            .version_req_satisfies(req_version_req, &nv.version)
            .ok()
            .unwrap_or(false)
      })
      .map(|(nv, id)| (nv.clone(), *id));

    let (pkg_nv, node_id) = match existing_root {
      Some(existing) => existing,
      None => {
        let (pkg_nv, node_id, _version_info) =
          Self::resolve_node_from_info_on_state(
            &mut state,
            self.api,
            &package_req.name,
            req_version_req,
            &version_resolver,
            &self.initial_overrides,
          )?;
        // Compute child overrides for this root package's subtree
        let child_overrides = self
          .initial_overrides
          .for_child(&pkg_nv.name, &pkg_nv.version);
        state.pending.push_back(PendingNode {
          node_id,
          ancestors: Vec::new(),
          active_overrides: child_overrides,
        });
        (pkg_nv, node_id)
      }
    };

    state
      .tree
      .package_reqs
      .insert(package_req.clone(), pkg_nv.clone());
    state.tree.root_packages.insert(pkg_nv.clone(), node_id);

    if let Some(reporter) = self.reporter {
      reporter.on_resolved(package_req, &pkg_nv);
    }

    Ok(pkg_nv)
  }

  /// Resolve version and create/reuse a node. Returns (nv, node_id, version_info).
  ///
  /// This is a static method operating on `DepTreeState` so it can be called
  /// inside `borrow_mut()` blocks without requiring `&mut self`.
  fn resolve_node_from_info_on_state(
    state: &mut DepTreeState,
    api: &TNpmRegistryApi,
    pkg_req_name: &str,
    version_req: &VersionReq,
    version_resolver: &NpmPackageVersionResolver,
    active_overrides: &Rc<NpmOverrides>,
  ) -> Result<
    (Rc<PackageNv>, DepTreeNodeId, Arc<NpmPackageVersionInfo>),
    NpmResolutionError,
  > {
    let info = version_resolver.resolve_best_package_version_info(
      version_req,
      state
        .tree
        .package_name_versions
        .entry(version_resolver.info().name.clone())
        .or_default()
        .iter(),
    )?;

    let nv = Rc::new(PackageNv {
      name: version_resolver.info().name.clone(),
      version: info.version.clone(),
    });

    let version_info = Arc::new(info.clone());

    // Check for existing node with same nv
    if let Some(node_id) = state.tree.find_node_for_nv(&nv) {
      return Ok((nv, node_id, version_info));
    }

    // Parse deps
    let deps = if let Some(deps) = state.dep_entry_cache.get(&nv) {
      deps.clone()
    } else {
      state.dep_entry_cache.store(nv.clone(), info)?
    };

    let node_id = state.tree.create_node(
      nv.clone(),
      deps,
      version_info.clone(),
      active_overrides.clone(),
    );

    debug!(
      "Resolved {}@{} to {}",
      pkg_req_name,
      version_req.version_text(),
      nv,
    );

    // Prefetch tarball immediately — version identity is final in Phase 1
    if let Some(dist) = &info.dist {
      api.prefetch_tarball(&nv, dist);
    }

    // Speculatively prefetch transitive deps
    if let Some(transitive_deps) = state.dep_entry_cache.get(&nv) {
      let transitive_deps = transitive_deps.clone();
      for transitive_dep in transitive_deps.iter() {
        api.prefetch_package_info(&transitive_dep.name);
      }
    }

    Ok((nv, node_id, version_info))
  }

  /// Resolve all pending nodes. Resolves regular dependencies only.
  pub async fn resolve_pending(&self) -> Result<(), NpmResolutionError> {
    let mut did_dedup = false;

    loop {
      let batch: Vec<_> =
        self.state.borrow_mut().pending.drain(..).collect();
      if batch.is_empty() {
        if self.should_dedup && !did_dedup {
          self.run_dedup_pass().await?;
          did_dedup = true;
          continue; // dedup may have added new pending nodes
        }
        break;
      }

      // Resolve ALL pending subtrees concurrently
      futures::future::try_join_all(
        batch.into_iter().map(|pending| self.resolve_subtree(pending)),
      )
      .await?;
    }

    // Auto-resolve peer deps that don't have any matching package in the tree.
    self.resolve_auto_peers().await?;

    Ok(())
  }

  /// Auto-resolve peer deps that aren't satisfied by any existing package.
  ///
  /// Some peer deps may not appear as regular deps anywhere in the tree
  /// (e.g. `vite` is only a peer dep of `@deno/vite-plugin`). These need
  /// to be resolved from the registry and added as root packages so that
  /// Phase 2 can find them in the parent context.
  async fn resolve_auto_peers(&self) -> Result<(), NpmResolutionError> {
    // Collect peer dep entries that may need auto-resolution.
    // We collect unique (name, version_req) pairs for all required peer deps.
    // After resolving each to the latest matching version from the registry,
    // we only add it to root_packages if that specific version doesn't already
    // exist in the tree. This matches v1 behavior where auto-resolution
    // always picks the latest version from the registry.
    let auto_peers = {
      let state = self.state.borrow();
      // NVs explicitly requested by the user (via package_reqs).
      let package_req_nvs: HashSet<PackageNv> = state
        .tree
        .package_reqs
        .values()
        .map(|nv| (**nv).clone())
        .collect();
      let mut auto_peers: Vec<(StackString, VersionReq)> = Vec::new();
      for node in state.tree.nodes.iter() {
        for dep in node.deps.iter() {
          if !matches!(
            dep.kind,
            NpmDependencyEntryKind::Peer
              | NpmDependencyEntryKind::OptionalPeer
          ) {
            continue;
          }
          let name: StackString = dep.name.as_str().into();

          // Skip if there's an explicitly-requested (user-provided) root
          // package that satisfies this peer dep. The user's explicit
          // version choice takes precedence over auto-resolution.
          let has_explicit_satisfying_root = state
            .tree
            .root_packages
            .keys()
            .filter(|nv| nv.name.as_str() == name.as_str())
            .filter(|nv| package_req_nvs.contains(nv.as_ref()))
            .any(|nv| {
              dep.version_req.tag().is_some()
                || dep.version_req.matches(&nv.version)
            });
          if has_explicit_satisfying_root {
            continue;
          }

          // When dedup is enabled, skip auto-resolution if an existing
          // version in the tree already satisfies this peer dep. The dedup
          // pass has already consolidated versions, so the existing version
          // is optimal. Phase 2 will find it through the tree structure.
          if self.should_dedup {
            if let Some(versions) =
              state.tree.package_name_versions.get(&name)
            {
              if versions.iter().any(|v| {
                dep.version_req.tag().is_some()
                  || dep.version_req.matches(v)
              }) {
                continue;
              }
            }
          }

          // Check if we already queued this name
          if auto_peers.iter().any(|(n, _)| *n == name) {
            continue;
          }
          if matches!(dep.kind, NpmDependencyEntryKind::OptionalPeer) {
            // Don't auto-resolve optional peers — they're optional
            continue;
          }
          auto_peers.push((name, dep.version_req.clone()));
        }
      }
      auto_peers
    }; // borrow released

    if auto_peers.is_empty() {
      return Ok(());
    }

    // Fetch all peer package infos concurrently
    let peer_infos: Vec<_> = futures::future::join_all(
      auto_peers.iter().map(|(name, _)| async move {
        (name.clone(), self.api.package_info(name.as_str()).await)
      }),
    )
    .await;

    // Create nodes synchronously under borrow_mut
    let new_pending = {
      let mut state = self.state.borrow_mut();
      let mut new_pending = Vec::new();

      for ((_peer_name, version_req), (_name, info_result)) in
        auto_peers.iter().zip(peer_infos)
      {
        let package_info = match info_result {
          Ok(info) => info,
          Err(NpmRegistryPackageInfoLoadError::PackageNotExists {
            ..
          }) => {
            continue;
          }
          Err(e) => return Err(e.into()),
        };

        let version_resolver =
          self.version_resolver.get_for_package(&package_info);

        // Resolve to the LATEST matching version from the registry,
        // ignoring existing versions in the tree. This matches v1 behavior
        // where auto-resolution always picks the latest, regardless of
        // what versions already exist from other packages' regular deps.
        let version_info = match version_resolver
          .resolve_best_package_version_info(
            version_req,
            std::iter::empty(),
          ) {
          Ok(info) => info,
          Err(_) => {
            // No version in the registry satisfies the peer dep's version
            // req. Skip — Phase 2 will resolve using whatever version
            // exists in the tree and emit an unmet peer dep diagnostic.
            continue;
          }
        };

        let nv = Rc::new(PackageNv {
          name: package_info.name.clone(),
          version: version_info.version.clone(),
        });

        // If this specific version already exists in the tree, skip.
        // It doesn't need auto-resolution — Phase 2 will find it through
        // the normal tree structure (as a child of its parent node).
        if state.tree.find_node_for_nv(&nv).is_some() {
          continue;
        }

        // Create a new node for this version
        let version_info = Arc::new(version_info.clone());
        let deps = if let Some(deps) = state.dep_entry_cache.get(&nv) {
          deps.clone()
        } else {
          state.dep_entry_cache.store(nv.clone(), &version_info)?
        };

        let node_id = state.tree.create_node(
          nv.clone(),
          deps,
          version_info.clone(),
          self.initial_overrides.clone(),
        );

        // Prefetch tarball
        if let Some(dist) = &version_info.dist {
          self.api.prefetch_tarball(&nv, dist);
        }

        // Add as root package so Phase 2 can find it in parent_pkgs
        state
          .tree
          .root_packages
          .entry(nv.clone())
          .or_insert(node_id);

        let child_overrides =
          self.initial_overrides.for_child(&nv.name, &nv.version);
        new_pending.push(PendingNode {
          node_id,
          ancestors: Vec::new(),
          active_overrides: child_overrides,
        });
      }
      new_pending
    }; // borrow_mut released

    if new_pending.is_empty() {
      // Nothing was auto-resolved (either no unmet peers, or no satisfying
      // versions exist in the registry). Don't recurse.
      return Ok(());
    }

    // Resolve the deps of the auto-resolved peers concurrently
    futures::future::try_join_all(
      new_pending
        .into_iter()
        .map(|pending| self.resolve_subtree(pending)),
    )
    .await?;

    // Recurse in case the newly added packages introduced new unresolved peers
    Box::pin(self.resolve_auto_peers()).await
  }

  /// Recursively resolve a subtree rooted at `pending`.
  ///
  /// Mirrors pnpm's recursive `Promise.all` pattern:
  /// 1. Read node deps (brief immutable borrow)
  /// 2. Fetch ALL dep manifests concurrently (no borrow held)
  /// 3. Create children synchronously (brief mutable borrow)
  /// 4. Recursively resolve ALL children concurrently
  fn resolve_subtree(
    &self,
    pending: PendingNode,
  ) -> Pin<Box<dyn Future<Output = Result<(), NpmResolutionError>> + '_>> {
    Box::pin(async move {
      // --- Phase A: Read node deps (brief immutable borrow) ---
      let (deps, parent_nv, active_overrides) = {
        let state = self.state.borrow();
        let node = &state.tree.nodes[pending.node_id.0 as usize];
        (
          node.deps.clone(),
          node.nv.clone(),
          pending.active_overrides.clone(),
        )
      }; // borrow released

      if deps.is_empty() {
        return Ok(());
      }

      // Collect regular (non-peer) deps
      let regular_deps: Vec<_> = deps
        .iter()
        .filter(|d| matches!(d.kind, NpmDependencyEntryKind::Dep))
        .collect();

      if regular_deps.is_empty() {
        // Only peer deps — no_peers stays false (peers exist), nothing to do
        return Ok(());
      }

      // --- Phase B: Fetch ALL dep manifests concurrently (no borrow held) ---
      // Also prefetch peer dep manifests for warming the cache
      let peer_prefetch_futures = deps.iter().filter_map(|dep| {
        if matches!(
          dep.kind,
          NpmDependencyEntryKind::Peer
            | NpmDependencyEntryKind::OptionalPeer
        ) {
          Some(dep.name.clone())
        } else {
          None
        }
      });
      for name in peer_prefetch_futures {
        self.api.prefetch_package_info(&name);
      }

      // Collect alias names needed for overrides
      let alias_names: Vec<Option<PackageName>> = regular_deps
        .iter()
        .map(|dep| {
          active_overrides
            .get_alias_for(&dep.name)
            .cloned()
        })
        .collect();

      // Fetch all regular dep manifests + alias manifests concurrently
      let manifest_futures = regular_deps.iter().map(|dep| {
        let name = dep.name.clone();
        async move { (name, self.api.package_info(&dep.name).await) }
      });

      let alias_futures = alias_names.iter().map(|alias| async move {
        match alias {
          Some(alias_name) => {
            Some(self.api.package_info(alias_name.as_str()).await)
          }
          None => None,
        }
      });

      let (manifests, alias_infos): (Vec<_>, Vec<_>) = futures::future::join(
        futures::future::join_all(manifest_futures),
        futures::future::join_all(alias_futures),
      )
      .await;

      // Speculatively prefetch grandchild package infos
      for (_, manifest_result) in &manifests {
        if let Ok(info) = manifest_result {
          let version_info = info
            .dist_tags
            .get("latest")
            .and_then(|v| info.versions.get(v))
            .or_else(|| info.versions.values().next());
          if let Some(vi) = version_info {
            for dep_name in vi.dependencies.keys() {
              self.api.prefetch_package_info(dep_name);
            }
            for dep_name in vi.optional_dependencies.keys() {
              self.api.prefetch_package_info(dep_name);
            }
          }
        }
      }

      // --- Phase C: Create children synchronously (brief mutable borrow) ---
      let children_to_resolve = {
        let mut state = self.state.borrow_mut();
        let mut children = Vec::new();
        let mut found_peer = false;
        let mut created_any_child = false;

        for (i, dep) in regular_deps.iter().enumerate() {
          let (ref _name, ref manifest_result) = manifests[i];
          let package_info = match manifest_result {
            Ok(info) => info,
            Err(_) => {
              continue;
            }
          };

          // Check if already resolved as a child of this node
          if state.tree.nodes[pending.node_id.0 as usize]
            .children
            .contains_key(&dep.bare_specifier)
          {
            continue;
          }

          // Use alias info if available
          let effective_info = match &alias_infos[i] {
            Some(Ok(alias_info)) => alias_info,
            _ => package_info,
          };
          let effective_version_resolver =
            self.version_resolver.get_for_package(effective_info);

          // Apply overrides
          let effective_req =
            match active_overrides.get_override_for(&dep.name, None) {
              Some(req) => req,
              None => {
                let natural_version = effective_version_resolver
                  .resolve_best_package_version_info(
                    &dep.version_req,
                    state
                      .tree
                      .package_name_versions
                      .entry(
                        effective_version_resolver.info().name.clone(),
                      )
                      .or_default()
                      .iter(),
                  )
                  .ok()
                  .map(|info| info.version.clone());
                match natural_version.as_ref().and_then(|v| {
                  active_overrides.get_override_for(&dep.name, Some(v))
                }) {
                  Some(req) => req,
                  None => &dep.version_req,
                }
              }
            };

          let (child_nv, child_id, _) =
            Self::resolve_node_from_info_on_state(
              &mut state,
              self.api,
              &dep.name,
              effective_req,
              &effective_version_resolver,
              &active_overrides,
            )?;

          // Skip self-dependencies
          if child_nv == parent_nv {
            continue;
          }

          // Check for circular dependency (ancestor has same nv)
          let is_circular =
            pending.ancestors.iter().any(|anc| **anc == *child_nv);

          state.tree.nodes[pending.node_id.0 as usize]
            .children
            .insert(dep.bare_specifier.clone(), child_id);
          created_any_child = true;

          if !is_circular {
            let mut child_ancestors = pending.ancestors.clone();
            child_ancestors.push(parent_nv.clone());
            // Compute override context for the child's subtree
            let child_overrides = active_overrides
              .for_child(&child_nv.name, &child_nv.version);
            children.push(PendingNode {
              node_id: child_id,
              ancestors: child_ancestors,
              active_overrides: child_overrides,
            });
          }

          if !found_peer {
            found_peer =
              !state.tree.nodes[child_id.0 as usize].no_peers;
          }
        }

        // Only update no_peers if we actually created children for this node.
        // If another concurrent subtree already resolved this node, all
        // children would already exist and no_peers was already set correctly.
        if created_any_child {
          // Handle peer dep markers
          if deps.iter().any(|d| {
            matches!(
              d.kind,
              NpmDependencyEntryKind::Peer
                | NpmDependencyEntryKind::OptionalPeer
            )
          }) {
            found_peer = true;
          }
          if !found_peer {
            state.tree.nodes[pending.node_id.0 as usize].no_peers = true;
          }
        }

        children
      }; // borrow_mut released

      // --- Phase D: Recursively resolve ALL children concurrently ---
      futures::future::try_join_all(
        children_to_resolve
          .into_iter()
          .map(|child| self.resolve_subtree(child)),
      )
      .await?;

      Ok(())
    })
  }

  /// Dedup pass: consolidate multiple versions of the same package
  /// where possible.
  async fn run_dedup_pass(&self) -> Result<(), NpmResolutionError> {
    debug!("Running npm dedup pass on dep tree.");

    // Phase 1: Collect version requirements (immutable borrow)
    let package_version_reqs_by_version = {
      let state = self.state.borrow();
      type VersionReqsByVersion = BTreeMap<Version, Vec<VersionReq>>;
      let mut package_version_reqs_by_version: HashMap<
        PackageName,
        VersionReqsByVersion,
      > = HashMap::with_capacity(state.tree.nodes.len());

      let mut seen_nodes: HashSet<DepTreeNodeId> =
        HashSet::with_capacity(state.tree.nodes.len());
      let mut pending_nodes: VecDeque<DepTreeNodeId> = Default::default();

      for (req, pkg_nv) in &state.tree.package_reqs {
        if let Some(&node_id) = state.tree.root_packages.get(pkg_nv) {
          package_version_reqs_by_version
            .entry(req.name.clone())
            .or_default()
            .entry(pkg_nv.version.clone())
            .or_default()
            .push(req.version_req.clone());
          if seen_nodes.insert(node_id) {
            pending_nodes.push_back(node_id);
          }
        }
      }

      // Walk tree collecting version requirements
      while let Some(node_id) = pending_nodes.pop_front() {
        let node = &state.tree.nodes[node_id.0 as usize];
        let deps = node.deps.clone();

        for dep in deps.iter() {
          if dep.kind != NpmDependencyEntryKind::Dep {
            continue;
          }
          if let Some(&child_id) = state.tree.nodes[node_id.0 as usize]
            .children
            .get(&dep.bare_specifier)
          {
            let child_nv = &state.tree.nodes[child_id.0 as usize].nv;
            package_version_reqs_by_version
              .entry(child_nv.name.clone())
              .or_default()
              .entry(child_nv.version.clone())
              .or_default()
              .push(dep.version_req.clone());
            if seen_nodes.insert(child_id) {
              pending_nodes.push_back(child_id);
            }
          }
        }
      }

      package_version_reqs_by_version
    }; // borrow released

    // Phase 2: Assign highest satisfying versions (needs async API calls)
    let mut consolidated_versions: BTreeMap<
      PackageName,
      HashMap<VersionReq, Version>,
    > = Default::default();

    let to_dedup: Vec<_> = package_version_reqs_by_version
      .into_iter()
      .filter(|(_, reqs)| reqs.len() > 1)
      .collect();
    let results = futures::future::join_all(
      to_dedup
        .iter()
        .map(|(name, reqs)| self.assign_highest_satisfying(name, reqs)),
    )
    .await;
    for ((name, _), versions) in to_dedup.into_iter().zip(results) {
      if !versions.is_empty() {
        consolidated_versions.insert(name, versions);
      }
    }

    if consolidated_versions.is_empty() {
      return Ok(());
    }

    debug!("Consolidating npm versions in dep tree.");

    // Phase 3: Apply consolidation (mutable borrow)
    {
      let mut state = self.state.borrow_mut();

      // Update package_name_versions
      for (package_name, final_versions) in &consolidated_versions {
        if let Some(versions) =
          state.tree.package_name_versions.get_mut(package_name)
        {
          versions.retain(|version| {
            final_versions.values().any(|v| v == version)
          });
        }
      }

      // Update root packages
      let mut added_root_nvs = Vec::new();
      let mut maybe_root_nvs_to_remove = Vec::new();
      for (pkg_req, pkg_nv) in &mut state.tree.package_reqs {
        if let Some(new_versions) = consolidated_versions.get(&pkg_req.name)
          && let Some(new_version) = new_versions.get(&pkg_req.version_req)
          && pkg_nv.version != *new_version
        {
          maybe_root_nvs_to_remove.push(pkg_nv.clone());
          let new_nv = Rc::new(PackageNv {
            name: pkg_nv.name.clone(),
            version: new_version.clone(),
          });
          *pkg_nv = new_nv.clone();
          added_root_nvs.push(new_nv);
        }
      }

      // Set root package nodes for new nvs
      for nv in &added_root_nvs {
        if let Some(node_id) = state.tree.find_node_for_nv(nv) {
          state.tree.root_packages.insert(nv.clone(), node_id);
        }
      }

      // Remove old root nvs no longer referenced
      for pkg_nv in &maybe_root_nvs_to_remove {
        if !state.tree.package_reqs.values().any(|v| v == pkg_nv) {
          state.tree.root_packages.remove(pkg_nv);
        }
      }

      // Clear consolidated children so they get re-resolved.
      let mut specifiers_to_remove: Vec<Vec<StackString>> =
        Vec::with_capacity(state.tree.nodes.len());
      for node in &state.tree.nodes {
        let deps = node.deps.clone();
        let mut to_remove = Vec::new();
        for dep in deps.iter() {
          if dep.kind != NpmDependencyEntryKind::Dep {
            continue;
          }
          if let Some(&child_id) = node.children.get(&dep.bare_specifier) {
            let child_nv = &state.tree.nodes[child_id.0 as usize].nv;
            if let Some(versions) =
              consolidated_versions.get(&child_nv.name)
              && versions.contains_key(&dep.version_req)
            {
              to_remove.push(dep.bare_specifier.clone());
            }
          }
        }
        specifiers_to_remove.push(to_remove);
      }
      let mut nodes_with_cleared_children = HashSet::new();
      for (i, node) in state.tree.nodes.iter_mut().enumerate() {
        node.no_peers = false;
        for specifier in &specifiers_to_remove[i] {
          node.children.remove(specifier);
          nodes_with_cleared_children.insert(DepTreeNodeId(i as u32));
        }
      }

      // Re-add all nodes that had children cleared to pending so they
      // get re-resolved. Collect first to avoid borrow conflict.
      let root_entries: Vec<_> = state
        .tree
        .root_packages
        .iter()
        .map(|(nv, &node_id)| (nv.clone(), node_id))
        .collect();
      for (nv, node_id) in &root_entries {
        let child_overrides =
          self.initial_overrides.for_child(&nv.name, &nv.version);
        state.pending.push_back(PendingNode {
          node_id: *node_id,
          ancestors: Vec::new(),
          active_overrides: child_overrides,
        });
      }
      let root_node_ids: HashSet<_> =
        root_entries.iter().map(|(_, id)| *id).collect();
      for node_id in nodes_with_cleared_children {
        // Don't double-add root packages (already added above)
        if root_node_ids.contains(&node_id) {
          continue;
        }
        // Use the node's stored overrides for non-root re-processing
        let overrides = state.tree.nodes[node_id.0 as usize]
          .active_overrides
          .clone();
        state.pending.push_back(PendingNode {
          node_id,
          ancestors: Vec::new(),
          active_overrides: overrides,
        });
      }
    } // borrow_mut released

    Ok(())
  }

  async fn assign_highest_satisfying(
    &self,
    package_name: &PackageName,
    by_version: &BTreeMap<Version, Vec<VersionReq>>,
  ) -> HashMap<VersionReq, Version> {
    // Package info should already be cached
    let package_info = self.api.package_info(package_name).await.unwrap();
    let version_resolver =
      self.version_resolver.get_for_package(&package_info);

    let reqs = by_version
      .values()
      .flat_map(|rs| rs.iter())
      .collect::<HashSet<_>>();

    let mut candidates: Vec<Version> = by_version.keys().cloned().collect();
    candidates.sort_by(|a, b| b.cmp(a));

    // Try one global winner
    if let Some(global) = candidates.iter().find(|v| {
      reqs.iter().all(|r| {
        version_resolver
          .version_req_satisfies(r, v)
          .ok()
          .unwrap_or(false)
      })
    }) {
      return reqs
        .iter()
        .map(|r| ((*r).clone(), global.clone()))
        .collect();
    }

    // Otherwise highest-first per-range
    let mut unassigned = reqs;
    let mut assigned: HashMap<VersionReq, Version> =
      HashMap::with_capacity(unassigned.len());

    for v in candidates.into_iter() {
      let matching = unassigned
        .iter()
        .filter(|r| {
          version_resolver
            .version_req_satisfies(r, &v)
            .ok()
            .unwrap_or(false)
        })
        .map(|v| (*v).clone())
        .collect::<Vec<_>>();

      if matching.is_empty() {
        continue;
      }

      for r in &matching {
        assigned.insert(r.clone(), v.clone());
        unassigned.remove(r);
      }
    }

    assigned
  }

  /// Consume the builder and return the frozen dep tree.
  pub fn into_dep_tree(self) -> DepTree {
    self.state.into_inner().tree
  }
}


#[cfg(test)]
mod tests;
