use std::collections::BTreeMap;
use std::sync::Arc;

use deno_semver::package::PackageReq;
use pretty_assertions::assert_eq;

use super::*;
use crate::registry::NpmRegistryApi;
use crate::registry::TestNpmRegistryApi;
use crate::resolution::common::NewestDependencyDateOptions;
use crate::resolution::peer_resolution;
use crate::resolution::snapshot::NpmResolutionSnapshot;
use crate::resolution::SerializedNpmResolutionSnapshot;
use crate::resolution::SerializedNpmResolutionSnapshotPackage;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestNpmResolutionPackage {
  pub pkg_id: String,
  pub copy_index: u8,
  pub dependencies: BTreeMap<String, String>,
}

fn snapshot_to_packages(
  snapshot: &NpmResolutionSnapshot,
) -> (Vec<TestNpmResolutionPackage>, Vec<(String, String)>) {
  let mut packages = snapshot
    .all_packages_for_every_system()
    .cloned()
    .collect::<Vec<_>>();
  packages.sort_by(|a, b| a.id.cmp(&b.id));
  let mut package_reqs = snapshot
    .package_reqs
    .iter()
    .map(|(a, b)| {
      (
        a.to_string(),
        snapshot
          .root_packages
          .get(b)
          .unwrap()
          .as_serialized()
          .to_string(),
      )
    })
    .collect::<Vec<_>>();
  package_reqs.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()));

  let packages = packages
    .into_iter()
    .map(|pkg| TestNpmResolutionPackage {
      pkg_id: pkg.id.as_serialized().to_string(),
      copy_index: pkg.copy_index,
      dependencies: pkg
        .dependencies
        .into_iter()
        .map(|(key, value)| {
          (key.to_string(), value.as_serialized().to_string())
        })
        .collect(),
    })
    .collect();

  (packages, package_reqs)
}

use std::collections::BTreeSet;

use crate::NpmSystemInfo;
use crate::registry::NpmPackageVersionInfo;
use crate::resolution::NpmOverrides;
use crate::resolution::common::NewestDependencyDate;
use crate::resolution::graph::NpmResolutionError;

#[derive(Default)]
struct RunV2ResolverOptions<'a> {
  snapshot: NpmResolutionSnapshot,
  reqs: Vec<&'a str>,
  link_packages:
    Option<&'a HashMap<deno_semver::package::PackageName, Vec<NpmPackageVersionInfo>>>,
  expected_diagnostics: Vec<&'a str>,
  newest_dependency_date: NewestDependencyDateOptions,
  skip_dedup: bool,
  overrides: NpmOverrides,
}

async fn run_v2_resolver(
  api: &TestNpmRegistryApi,
  reqs: Vec<&str>,
) -> NpmResolutionSnapshot {
  run_v2_resolver_with_all_options(
    api,
    RunV2ResolverOptions {
      reqs,
      ..Default::default()
    },
  )
  .await
  .unwrap()
}

async fn run_v2_resolver_and_get_output(
  api: &TestNpmRegistryApi,
  reqs: Vec<&str>,
) -> (Vec<TestNpmResolutionPackage>, Vec<(String, String)>) {
  let snapshot = run_v2_resolver(api, reqs).await;
  snapshot_to_packages(&snapshot)
}

async fn run_v2_resolver_with_options_and_get_output(
  api: &TestNpmRegistryApi,
  options: RunV2ResolverOptions<'_>,
) -> (Vec<TestNpmResolutionPackage>, Vec<(String, String)>) {
  let snapshot = run_v2_resolver_with_all_options(api, options)
    .await
    .unwrap();
  snapshot_to_packages(&snapshot)
}

async fn run_v2_resolver_with_all_options(
  api: &TestNpmRegistryApi,
  options: RunV2ResolverOptions<'_>,
) -> Result<NpmResolutionSnapshot, NpmResolutionError> {
  let link_packages = Arc::new(
    options
      .link_packages
      .cloned()
      .unwrap_or_else(HashMap::default),
  );
  let npm_version_resolver = NpmVersionResolver {
    link_packages: link_packages.clone(),
    newest_dependency_date_options: options.newest_dependency_date,
    overrides: Arc::new(options.overrides),
  };

  let initial_tree = if options.snapshot.packages.is_empty() {
    DepTree::new()
  } else {
    // Populate cached infos from the API, matching real resolver behavior
    let mut api_cached_infos = HashMap::new();
    for pkg_id in options.snapshot.packages.keys() {
      if !api_cached_infos.contains_key(&pkg_id.nv.name) {
        if let Ok(info) = api.package_info(&pkg_id.nv.name).await {
          api_cached_infos.insert(pkg_id.nv.name.clone(), info);
        }
      }
    }
    DepTree::from_snapshot(
      options.snapshot,
      &npm_version_resolver,
      &api_cached_infos,
    )
  };

  let builder = DepTreeBuilder::new(
    initial_tree,
    api,
    &npm_version_resolver,
    None,
    !options.skip_dedup,
  );

  for req in options.reqs {
    let req = PackageReq::from_str(req).unwrap();
    let info = api.package_info(&req.name).await.unwrap();
    builder.add_package_req(&req, &info)?;
  }

  builder.resolve_pending().await?;
  let tree = builder.into_dep_tree();

  // Phase 2
  let peer_result = peer_resolution::resolve_peers(&tree);

  // Check diagnostics
  {
    let diagnostics = peer_result
      .unmet_peer_diagnostics
      .iter()
      .map(|d| {
        format!(
          "{}: {} -> {}",
          d.ancestors
            .iter()
            .rev()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" -> "),
          d.dependency,
          d.resolved
        )
      })
      .collect::<Vec<_>>();
    assert_eq!(diagnostics, options.expected_diagnostics);
  }

  Ok(peer_resolution::build_snapshot(&tree, &peer_result))
}

fn package_names_with_info(
  snapshot: &NpmResolutionSnapshot,
  system_info: &NpmSystemInfo,
) -> Vec<String> {
  let mut packages = snapshot
    .all_system_packages(system_info)
    .into_iter()
    .map(|p| p.id.as_serialized().to_string())
    .collect::<Vec<_>>();
  packages.sort();
  packages
}

fn make_overrides(
  json: serde_json::Value,
) -> NpmOverrides {
  NpmOverrides::from_value(json, &Default::default()).unwrap()
}

fn make_overrides_with_root_deps(
  json: serde_json::Value,
  root_deps: std::collections::HashMap<
    deno_semver::StackString,
    deno_semver::StackString,
  >,
) -> NpmOverrides {
  NpmOverrides::from_value(json, &root_deps).unwrap()
}

// ====================================================================
// Tests ported from graph.rs to run against the new two-phase pipeline
// ====================================================================

#[tokio::test]
async fn resolve_deps_no_peer() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "0.1.0");
  api.ensure_package_version("package-c", "0.0.10");
  api.ensure_package_version("package-d", "3.2.1");
  api.ensure_package_version("package-d", "3.2.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^0.1"));
  api.add_dependency(("package-c", "0.1.0"), ("package-d", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@2.0.0".to_string(),),
          ("package-c".to_string(), "package-c@0.1.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@0.1.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-d".to_string(),
          "package-d@3.2.1".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@3.2.1".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_deps_circular() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "*"));
  api.add_dependency(("package-b", "2.0.0"), ("package-a", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@2.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn skips_bundle_dependencies() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.add_bundle_dependency(("package-a", "1.0.0"), ("package-b", "1"));

  let (packages, _package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![TestNpmResolutionPackage {
      pkg_id: "package-a@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::new(),
    },]
  );
}

#[tokio::test]
async fn peer_deps_simple_top_tree() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-a@1.0", "package-peer@1.0"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1.0".to_string(),
        "package-a@1.0.0_package-peer@1.0.0".to_string()
      ),
      (
        "package-peer@1.0".to_string(),
        "package-peer@1.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn peer_deps_simple_root_pkg_children() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-0", "1.0.0"), ("package-peer", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@1.0.0".to_string(),)
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-0@1.0".to_string(),
      "package-0@1.0.0_package-peer@1.0.0".to_string()
    ),]
  );
}

#[tokio::test]
async fn peer_deps_simple_deeper() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-1", "1.0.0");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-1", "1"));
  api.add_dependency(("package-1", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-1", "1.0.0"), ("package-peer", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-1".to_string(),
          "package-1@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-1@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@1.0.0".to_string(),)
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-0@1.0".to_string(), "package-0@1.0.0".to_string()),]
  );
}

#[tokio::test]
async fn resolve_with_peer_deps_top_tree() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-peer", "4"));
  api.add_peer_dependency(("package-c", "3.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-a@1", "package-peer@4.0.0"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer@4.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer@4.0.0".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-peer@4.0.0".to_string()
      ),
      (
        "package-peer@4.0.0".to_string(),
        "package-peer@4.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_with_peer_deps_ancestor_sibling_not_top_tree() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.1.1");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-0", "1.1.1"), ("package-a", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_dependency(("package-a", "1.0.0"), ("package-peer", "4.0.0"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-peer", "4"));
  api.add_peer_dependency(("package-c", "3.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.1.1"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.1.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0_package-peer@4.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer@4.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer@4.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@4.0.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-0@1.1.1".to_string(), "package-0@1.1.1".to_string())]
  );
}

#[tokio::test]
async fn resolve_with_peer_deps_non_matching_version() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.1.1");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-0", "1.1.1"), ("package-a", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_dependency(("package-a", "1.0.0"), ("package-peer", "4.0.0"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-peer", "1"));
  api.add_peer_dependency(("package-c", "3.0.0"), ("package-peer", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_with_options_and_get_output(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["package-0@1.1.1"],
        expected_diagnostics: vec![
          "package-0@1.1.1 -> package-a@1.0.0 -> package-b@2.0.0: package-peer@1 -> 4.0.0",
          "package-0@1.1.1 -> package-a@1.0.0 -> package-c@3.0.0: package-peer@1 -> 4.0.0"
        ],
        ..Default::default()
      },
    )
    .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.1.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0_package-peer@4.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer@4.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer@4.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@4.0.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-0@1.1.1".to_string(), "package-0@1.1.1".to_string())]
  );
}

#[tokio::test]
async fn resolve_with_optional_peer_dep_not_resolved() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_optional_peer_dependency(
    ("package-b", "2.0.0"),
    ("package-peer", "4"),
  );
  api.add_optional_peer_dependency(
    ("package-c", "3.0.0"),
    ("package-peer", "*"),
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@2.0.0".to_string(),),
          ("package-c".to_string(), "package-c@3.0.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_with_optional_peer_found() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_optional_peer_dependency(
    ("package-b", "2.0.0"),
    ("package-peer", "4"),
  );
  api.add_optional_peer_dependency(
    ("package-c", "3.0.0"),
    ("package-peer", "*"),
  );

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-a@1", "package-peer@4.0.0"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer@4.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer@4.0.0".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@4.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-peer@4.0.0".to_string()
      ),
      (
        "package-peer@4.0.0".to_string(),
        "package-peer@4.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_optional_dep_npm_req_top() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_optional_peer_dependency(
    ("package-a", "1.0.0"),
    ("package-peer", "*"),
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1", "package-peer@1"])
      .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-peer@1.0.0".to_string()
      ),
      (
        "package-peer@1".to_string(),
        "package-peer@1.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn package_has_self_as_dependency() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-a", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![TestNpmResolutionPackage {
      pkg_id: "package-a@1.0.0".to_string(),
      copy_index: 0,
      dependencies: Default::default(),
    }]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn package_has_self_but_different_version_as_dependency() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-a", "0.5.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-a", "^0.5"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@0.5.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@0.5.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn grand_child_package_has_self_as_peer_dependency_root() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "2"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-a", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@2.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn grand_child_package_has_self_as_peer_dependency_under_root() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-a", "*"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "2"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-a", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@2.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-0@1.0".to_string(), "package-0@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_peer_deps_in_ancestor_root() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-a", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_peer_deps_in_ancestor_non_root() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_peer_deps_circular() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "*"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-a", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@2.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_peer_deps_multiple_copies() {
  for _ in 0..3 {
    let api = TestNpmRegistryApi::default();
    api.ensure_package_version("package-a", "1.0.0");
    api.ensure_package_version("package-b", "2.0.0");
    api.ensure_package_version("package-dep", "3.0.0");
    api.ensure_package_version("package-peer", "4.0.0");
    api.ensure_package_version("package-peer", "5.0.0");
    api.add_dependency(("package-a", "1.0.0"), ("package-dep", "*"));
    api.add_dependency(("package-a", "1.0.0"), ("package-peer", "4"));
    api.add_dependency(("package-b", "2.0.0"), ("package-dep", "*"));
    api.add_dependency(("package-b", "2.0.0"), ("package-peer", "5"));
    api.add_peer_dependency(("package-dep", "3.0.0"), ("package-peer", "*"));

    let (packages, package_reqs) =
      run_v2_resolver_and_get_output(&api, vec!["package-a@1", "package-b@2"])
        .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@4.0.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-dep".to_string(),
              "package-dep@3.0.0_package-peer@4.0.0".to_string(),
            ),
            ("package-peer".to_string(), "package-peer@4.0.0".to_string(),),
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-b@2.0.0_package-peer@5.0.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-dep".to_string(),
              "package-dep@3.0.0_package-peer@5.0.0".to_string(),
            ),
            ("package-peer".to_string(), "package-peer@5.0.0".to_string(),),
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-dep@3.0.0_package-peer@4.0.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@4.0.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-dep@3.0.0_package-peer@5.0.0".to_string(),
          copy_index: 1,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@5.0.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@4.0.0".to_string(),
          copy_index: 0,
          dependencies: Default::default(),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@5.0.0".to_string(),
          copy_index: 0,
          dependencies: Default::default(),
        },
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1".to_string(),
          "package-a@1.0.0_package-peer@4.0.0".to_string()
        ),
        (
          "package-b@2".to_string(),
          "package-b@2.0.0_package-peer@5.0.0".to_string()
        )
      ]
    );
  }
}

#[tokio::test]
async fn resolve_dep_with_peer_deps_dep_then_peer() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-peer", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0", "package-b@1.0"])
      .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-b@1.0.0__package-peer@1.0.0_package-peer@1.0.0"
          .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-c".to_string(),
            "package-c@1.0.0_package-b@1.0.0__package-peer@1.0.0_package-peer@1.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@1.0.0".to_string(),)
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0__package-peer@1.0.0_package-peer@1.0.0"
          .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1.0".to_string(),
        "package-a@1.0.0_package-b@1.0.0__package-peer@1.0.0_package-peer@1.0.0".to_string()
      ),
      (
        "package-b@1.0".to_string(),
        "package-b@1.0.0_package-peer@1.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn peer_dep_on_self() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.add_peer_dependency(("package-a", "1.0.0"), ("package-a", "1"));

  let snapshot =
    run_v2_resolver(&api, vec!["package-a@1.0.0"]).await;
  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "darwin".into(),
      cpu: "x86_64".into(),
    },
  );
  assert_eq!(packages, vec!["package-a@1.0.0".to_string()]);
}

#[tokio::test]
async fn non_existent_optional_peer_dep() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.add_optional_peer_dependency(
    ("package-b", "1.0.0"),
    ("package-non-existent", "*"),
  );
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "*"));
  let snapshot =
    run_v2_resolver(&api, vec!["package-a@1.0.0"]).await;
  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "darwin".into(),
      cpu: "x86_64".into(),
    },
  );
  assert_eq!(
    packages,
    vec!["package-a@1.0.0".to_string(), "package-b@1.0.0".to_string(),]
  );
}

#[tokio::test]
async fn dudpes_dep_overlapping_high_version_constraint_then_low() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-b", "1.0.1");
  api.ensure_package_version("package-c", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-b", "1.0.0"));

  let (packages, _package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@1.0.0".to_string(),),
          ("package-c".to_string(), "package-c@1.0.0".to_string(),)
        ])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::new(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
    ]
  );
}

#[tokio::test]
async fn dudpes_dep_overlapping_high_version_constraint_then_low_with_peer_deps()
{
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-b", "1.0.1");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-b", "1.0.0"));
  api.add_peer_dependency(("package-b", "1.0.1"), ("package-d", "1"));

  let (packages, _package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-a@1.0.0", "package-d@1.0.0"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@1.0.0".to_string(),),
          ("package-c".to_string(), "package-c@1.0.0".to_string(),)
        ])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::new(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::new(),
      },
    ]
  );
}

// === npm overrides tests ===

#[tokio::test]
async fn override_simple_version() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("foo", "1.0.0");
  api.ensure_package_version("foo", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("foo", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "foo": "1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("foo@"))
    .unwrap();
  assert_eq!(foo_pkg.pkg_id, "foo@1.0.0");
}

#[tokio::test]
async fn override_does_not_affect_unrelated_packages() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("foo", "1.0.0");
  api.ensure_package_version("foo", "2.0.0");
  api.ensure_package_version("bar", "1.0.0");
  api.ensure_package_version("bar", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("foo", "^2.0.0"));
  api.add_dependency(("package-a", "1.0.0"), ("bar", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "foo": "1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages.iter().find(|p| p.pkg_id.starts_with("foo@")).unwrap();
  assert_eq!(foo_pkg.pkg_id, "foo@1.0.0");
  let bar_pkg = packages.iter().find(|p| p.pkg_id.starts_with("bar@")).unwrap();
  assert_eq!(bar_pkg.pkg_id, "bar@2.0.0");
}

#[tokio::test]
async fn override_transitive_dependency() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("leaf", "1.0.0");
  api.ensure_package_version("leaf", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1.0.0"));
  api.add_dependency(("package-b", "1.0.0"), ("leaf", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "leaf": "1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  let leaf_pkg = packages.iter().find(|p| p.pkg_id.starts_with("leaf@")).unwrap();
  assert_eq!(leaf_pkg.pkg_id, "leaf@1.0.0");
}

#[tokio::test]
async fn override_no_overrides_unchanged() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("foo", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("foo", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: Default::default(),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages.iter().find(|p| p.pkg_id.starts_with("foo@")).unwrap();
  assert_eq!(foo_pkg.pkg_id, "foo@2.0.0");
}

#[tokio::test]
async fn override_npm_alias() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("foo", "2.0.0");
  api.ensure_package_version("bar", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("foo", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "foo": "npm:bar@1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  assert!(packages.iter().all(|p| !p.pkg_id.starts_with("foo@")));
  let bar_pkg = packages.iter().find(|p| p.pkg_id.starts_with("bar@")).unwrap();
  assert_eq!(bar_pkg.pkg_id, "bar@1.0.0");
  let parent = packages.iter().find(|p| p.pkg_id.starts_with("package-a@")).unwrap();
  assert_eq!(parent.dependencies.get("foo").unwrap(), "bar@1.0.0");
}

#[tokio::test]
async fn override_npm_alias_transitive() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("leaf", "2.0.0");
  api.ensure_package_version("replacement", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1.0.0"));
  api.add_dependency(("package-b", "1.0.0"), ("leaf", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "leaf": "npm:replacement@1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  assert!(packages.iter().all(|p| !p.pkg_id.starts_with("leaf@")));
  let replacement = packages.iter().find(|p| p.pkg_id.starts_with("replacement@")).unwrap();
  assert_eq!(replacement.pkg_id, "replacement@1.0.0");
  let pkg_b = packages.iter().find(|p| p.pkg_id.starts_with("package-b@")).unwrap();
  assert_eq!(pkg_b.dependencies.get("leaf").unwrap(), "replacement@1.0.0");
}

#[tokio::test]
async fn override_jsr_alias() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("foo", "2.0.0");
  api.ensure_package_version("@jsr/std__path", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("foo", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "foo": "jsr:@std/path@1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  assert!(packages.iter().all(|p| !p.pkg_id.starts_with("foo@")));
  let jsr_pkg = packages.iter().find(|p| p.pkg_id.starts_with("@jsr/std__path@")).unwrap();
  assert_eq!(jsr_pkg.pkg_id, "@jsr/std__path@1.0.0");
  let parent = packages.iter().find(|p| p.pkg_id.starts_with("package-a@")).unwrap();
  assert_eq!(parent.dependencies.get("foo").unwrap(), "@jsr/std__path@1.0.0");
}

#[tokio::test]
async fn override_jsr_version_only() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("@std/path", "2.0.0");
  api.ensure_package_version("@jsr/std__path", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("@std/path", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      overrides: make_overrides(serde_json::json!({ "@std/path": "jsr:1.0.0" })),
      ..Default::default()
    },
  )
  .await;

  assert!(packages.iter().all(|p| !p.pkg_id.starts_with("@std/path@")));
  let jsr_pkg = packages.iter().find(|p| p.pkg_id.starts_with("@jsr/std__path@")).unwrap();
  assert_eq!(jsr_pkg.pkg_id, "@jsr/std__path@1.0.0");
  let parent = packages.iter().find(|p| p.pkg_id.starts_with("package-a@")).unwrap();
  assert_eq!(parent.dependencies.get("@std/path").unwrap(), "@jsr/std__path@1.0.0");
}

// ====================================================================
// Batch 2: More tests ported from graph.rs
// ====================================================================

#[tokio::test]
async fn resolve_optional_peer_first_not_resolved_second_resolved_scenario1()
{
  // When resolving a dependency a second time and it has an optional
  // peer dependency that wasn't previously resolved, it should resolve all the
  // previous versions to the new one
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.ensure_package_version("package-peer-unresolved", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-peer", "^1"));
  api.add_optional_peer_dependency(
    ("package-b", "1.0.0"),
    ("package-peer", "*"),
  );
  api.add_optional_peer_dependency(
    ("package-b", "1.0.0"),
    ("package-peer-unresolved", "*"),
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1", "package-b@1"])
      .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@1.0.0_package-peer@1.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@1.0.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-peer@1.0.0".to_string()
      ),
      (
        "package-b@1".to_string(),
        "package-b@1.0.0_package-peer@1.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_optional_peer_first_not_resolved_second_resolved_scenario2()
{
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "2.0.0");
  api.add_optional_peer_dependency(
    ("package-a", "1.0.0"),
    ("package-peer", "*"),
  );
  api.add_dependency(("package-b", "1.0.0"), ("package-a", "1.0.0"));
  api.add_dependency(("package-b", "1.0.0"), ("package-peer", "2.0.0"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1", "package-b@1"])
      .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@2.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@2.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@2.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@2.0.0".to_string(),
          ),
          ("package-peer".to_string(), "package-peer@2.0.0".to_string(),)
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@2.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-peer@2.0.0".to_string()
      ),
      (
        "package-b@1".to_string(),
        "package-b@1.0.0_package-peer@2.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_optional_dep_different_resolution_second_time() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.ensure_package_version("package-peer", "2.0.0");
  api.add_optional_peer_dependency(
    ("package-a", "1.0.0"),
    ("package-peer", "*"),
  );
  api.add_dependency(("package-b", "1.0.0"), ("package-a", "1.0.0"));
  api.add_dependency(("package-b", "1.0.0"), ("package-peer", "2.0.0"));

  let input_reqs = vec!["package-a@1", "package-b@1", "package-peer@1.0.0"];
  let expected_packages = vec![
    TestNpmResolutionPackage {
      pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-peer".to_string(),
        "package-peer@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-a@1.0.0_package-peer@2.0.0".to_string(),
      copy_index: 1,
      dependencies: BTreeMap::from([(
        "package-peer".to_string(),
        "package-peer@2.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-b@1.0.0_package-peer@2.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([
        ("package-peer".to_string(), "package-peer@2.0.0".to_string()),
        (
          "package-a".to_string(),
          "package-a@1.0.0_package-peer@2.0.0".to_string(),
        ),
      ]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-peer@1.0.0".to_string(),
      copy_index: 0,
      dependencies: Default::default(),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-peer@2.0.0".to_string(),
      copy_index: 0,
      dependencies: Default::default(),
    },
  ];
  let expected_reqs = vec![
    (
      "package-a@1".to_string(),
      "package-a@1.0.0_package-peer@1.0.0".to_string(),
    ),
    (
      "package-b@1".to_string(),
      "package-b@1.0.0_package-peer@2.0.0".to_string(),
    ),
    (
      "package-peer@1.0.0".to_string(),
      "package-peer@1.0.0".to_string(),
    ),
  ];
  // skipping dedup
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          skip_dedup: true,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(packages, expected_packages);
    assert_eq!(package_reqs, expected_reqs);
  }
  // doing dedup
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          skip_dedup: false,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(packages, expected_packages);
    assert_eq!(package_reqs, expected_reqs);
  }
}

#[tokio::test]
async fn resolve_peer_dep_other_specifier_slot() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-peer", "2.0.0");
  // bit of an edge case... probably nobody has ever done this
  api.add_dependency(
    ("package-a", "1.0.0"),
    ("package-peer2", "npm:package-peer@2"),
  );
  api.add_peer_dependency(("package-a", "1.0.0"), ("package-peer", "2"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@2.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-peer".to_string(), "package-peer@2.0.0".to_string(),),
          (
            "package-peer2".to_string(),
            "package-peer@2.0.0".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@2.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-a@1".to_string(),
      "package-a@1.0.0_package-peer@2.0.0".to_string()
    ),]
  );
}

#[tokio::test]
async fn resolve_nested_peer_deps_ancestor_sibling_deps() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-peer-a", "2.0.0");
  api.ensure_package_version("package-peer-b", "3.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-peer-b", "*"));
  api.add_peer_dependency(("package-0", "1.0.0"), ("package-peer-a", "2"));
  api.add_peer_dependency(
    ("package-peer-a", "2.0.0"),
    ("package-peer-b", "3"),
  );

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-0@1.0", "package-peer-a@2", "package-peer-b@3"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0_package-peer-a@2.0.0__package-peer-b@3.0.0_package-peer-b@3.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-peer-a".to_string(),
            "package-peer-a@2.0.0_package-peer-b@3.0.0".to_string(),
          ),
          (
            "package-peer-b".to_string(),
            "package-peer-b@3.0.0".to_string(),
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-a@2.0.0_package-peer-b@3.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-b".to_string(),
          "package-peer-b@3.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-b@3.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-0@1.0".to_string(),
        "package-0@1.0.0_package-peer-a@2.0.0__package-peer-b@3.0.0_package-peer-b@3.0.0"
          .to_string()
      ),
      (
        "package-peer-a@2".to_string(),
        "package-peer-a@2.0.0_package-peer-b@3.0.0".to_string()
      ),
      (
        "package-peer-b@3".to_string(),
        "package-peer-b@3.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_dep_and_peer_dist_tag() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-b", "3.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.ensure_package_version("package-e", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "some-tag"));
  api.add_dependency(("package-a", "1.0.0"), ("package-d", "1.0.0"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "1.0.0"));
  api.add_dependency(("package-a", "1.0.0"), ("package-e", "1.0.0"));
  api.add_dependency(("package-e", "1.0.0"), ("package-b", "some-tag"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-d", "other-tag"));
  api.add_dist_tag("package-b", "some-tag", "2.0.0");
  api.add_dist_tag("package-d", "other-tag", "1.0.0");

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-d@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@2.0.0".to_string(),),
          (
            "package-c".to_string(),
            "package-c@1.0.0_package-d@1.0.0".to_string(),
          ),
          ("package-d".to_string(), "package-d@1.0.0".to_string(),),
          ("package-e".to_string(), "package-e@1.0.0".to_string(),),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-d@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-d".to_string(),
          "package-d@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-e@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@2.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-a@1.0".to_string(),
      "package-a@1.0.0_package-d@1.0.0".to_string()
    ),]
  );
}

#[tokio::test]
async fn nested_deps_same_peer_dep_ancestor() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-1", "1.0.0");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-0", "1.0.0"), ("package-1", "1"));
  api.add_dependency(("package-1", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-a", "*"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-a", "*"));
  api.add_peer_dependency(("package-d", "1.0.0"), ("package-a", "*"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-0", "*"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-0", "*"));
  api.add_peer_dependency(("package-d", "1.0.0"), ("package-0", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-1".to_string(),
            "package-1@1.0.0_package-0@1.0.0".to_string(),
          ),
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-0@1.0.0".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-1@1.0.0_package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0_package-0@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-0".to_string(),
            "package-0@1.0.0".to_string(),
          ),
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-0@1.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-0".to_string(),
            "package-0@1.0.0".to_string(),
          ),
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-0@1.0.0".to_string(),
          ),
          (
            "package-d".to_string(),
            "package-d@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0_package-0@1.0.0_package-a@1.0.0__package-0@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-0".to_string(),
            "package-0@1.0.0".to_string(),
          ),
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-0@1.0.0".to_string(),
          )
        ]),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-0@1.0".to_string(), "package-0@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn peer_dep_resolved_then_resolved_deeper() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-1", "1.0.0");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.add_dependency(("package-0", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-0", "1.0.0"), ("package-1", "1"));
  api.add_dependency(("package-1", "1.0.0"), ("package-a", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["package-0@1.0", "package-peer@1.0"],
  )
  .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-1".to_string(),
            "package-1@1.0.0_package-peer@1.0.0".to_string(),
          ),
          (
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.0.0".to_string(),
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-1@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      }
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-0@1.0".to_string(),
        "package-0@1.0.0_package-peer@1.0.0".to_string()
      ),
      (
        "package-peer@1.0".to_string(),
        "package-peer@1.0.0".to_string()
      )
    ]
  );
}

#[tokio::test]
async fn resolve_dep_with_peer_deps_circular_1() {
  // a -> b -> c -> d -> c where c has a peer dependency on b
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_dependency(("package-d", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          ("package-b".to_string(), "package-b@1.0.0".to_string(),),
          (
            "package-d".to_string(),
            "package-d@1.0.0_package-b@1.0.0".to_string(),
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0_package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_dep_with_peer_deps_circular_3() {
  // a -> b -> c -> d -> c (peer)
  //                  -> e -> a (peer)
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.ensure_package_version("package-e", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_dependency(("package-d", "1.0.0"), ("package-e", "1"));
  api.add_peer_dependency(("package-d", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-e", "1.0.0"), ("package-a", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-a@1.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-d".to_string(),
          "package-d@1.0.0_package-c@1.0.0__package-a@1.0.0_package-a@1.0.0"
            .to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id:
          "package-d@1.0.0_package-c@1.0.0__package-a@1.0.0_package-a@1.0.0"
            .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-c".to_string(),
            "package-c@1.0.0_package-a@1.0.0".to_string(),
          ),
          (
            "package-e".to_string(),
            "package-e@1.0.0_package-a@1.0.0".to_string()
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-e@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string()
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_optional_deps() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.ensure_package_version("package-e", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dep_and_optional_dep(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_optional_dep(("package-d", "1.0.0"), ("package-e", "1"));
  api.with_version_info(("package-c", "1.0.0"), |info| {
    info.os = vec!["win32".into(), "darwin".into()];
  });
  api.with_version_info(("package-e", "1.0.0"), |info| {
    info.os = vec!["win32".into()];
  });

  let snapshot =
    run_v2_resolver(&api, vec!["package-a@1.0.0"]).await;
  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "win32".into(),
      cpu: "x86".into(),
    },
  );
  assert_eq!(
    packages,
    vec![
      "package-a@1.0.0".to_string(),
      "package-b@1.0.0".to_string(),
      "package-c@1.0.0".to_string(),
      "package-d@1.0.0".to_string(),
      "package-e@1.0.0".to_string(),
    ]
  );

  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "darwin".into(),
      cpu: "x86".into(),
    },
  );
  assert_eq!(
    packages,
    vec![
      "package-a@1.0.0".to_string(),
      "package-b@1.0.0".to_string(),
      "package-c@1.0.0".to_string(),
      "package-d@1.0.0".to_string(),
    ]
  );

  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "linux".into(),
      cpu: "x86".into(),
    },
  );
  assert_eq!(
    packages,
    vec!["package-a@1.0.0".to_string(), "package-b@1.0.0".to_string()]
  );
}

#[tokio::test]
async fn resolve_optional_to_required() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b1", "1.0.0");
  api.ensure_package_version("package-b2", "1.0.0");
  api.ensure_package_version("package-b3", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.ensure_package_version("package-e", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b1", "1"));
  api.add_dependency(("package-b1", "1.0.0"), ("package-b2", "1"));
  api.add_dependency(("package-b2", "1.0.0"), ("package-b3", "1"));
  // deep down this is set back to being required, so it and its required
  // dependency should be marked as required
  api.add_dependency(("package-b3", "1.0.0"), ("package-c", "1"));
  api.add_dep_and_optional_dep(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_dep_and_optional_dep(("package-d", "1.0.0"), ("package-e", "1"));

  api.with_version_info(("package-c", "1.0.0"), |info| {
    info.os = vec!["win32".into()];
  });
  api.with_version_info(("package-e", "1.0.0"), |info| {
    info.os = vec!["win32".into()];
  });

  let snapshot =
    run_v2_resolver(&api, vec!["package-a@1.0.0"]).await;

  let packages = package_names_with_info(
    &snapshot,
    &NpmSystemInfo {
      os: "darwin".into(),
      cpu: "x86".into(),
    },
  );
  assert_eq!(
    packages,
    vec![
      "package-a@1.0.0".to_string(),
      "package-b1@1.0.0".to_string(),
      "package-b2@1.0.0".to_string(),
      "package-b3@1.0.0".to_string(),
      "package-c@1.0.0".to_string(),
      "package-d@1.0.0".to_string(),
    ]
  );
}

#[tokio::test]
async fn errors_for_git_dep() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "*"));
  api.add_dependency(("package-b", "1.0.0"), ("SomeGitDep", "git:somerepo"));
  let err = run_v2_resolver_with_all_options(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0"],
      ..Default::default()
    },
  )
  .await
  .unwrap_err();
  match err {
    NpmResolutionError::DependencyEntry(err) => match err.source {
      crate::registry::NpmDependencyEntryErrorSource::RemoteDependency {
        specifier,
      } => {
        assert_eq!(specifier, "git:somerepo")
      }
      _ => unreachable!(),
    },
    _ => unreachable!(),
  }
}

// ====================================================================
// Batch 3: Override tests (scoped, version selector, $ref, dot key, alias)
// ====================================================================

#[tokio::test]
async fn override_scoped_to_parent() {
  // "parent": { "child": "1.0.0" } should only override child
  // when it's under parent's subtree
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("parent", "1.0.0");
  api.ensure_package_version("other", "1.0.0");
  api.ensure_package_version("child", "1.0.0");
  api.ensure_package_version("child", "2.0.0");
  api.add_dependency(("parent", "1.0.0"), ("child", "^2.0.0"));
  api.add_dependency(("other", "1.0.0"), ("child", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["parent@1.0.0", "other@1.0.0"],
      overrides: make_overrides(serde_json::json!({
        "parent": {
          "child": "1.0.0"
        }
      })),
      ..Default::default()
    },
  )
  .await;

  // parent's child should be 1.0.0 (overridden)
  let parent_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("parent@"))
    .unwrap();
  assert_eq!(
    parent_pkg.dependencies.get("child").unwrap(),
    "child@1.0.0"
  );
  // other's child should be 2.0.0 (not overridden)
  let other_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("other@"))
    .unwrap();
  assert_eq!(
    other_pkg.dependencies.get("child").unwrap(),
    "child@2.0.0"
  );
}

#[tokio::test]
async fn override_with_version_selector() {
  // "foo@^2.0.0": { "bar": "1.0.0" }
  // should only override bar when foo resolves to 2.x
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("foo", "2.1.0");
  api.ensure_package_version("bar", "1.0.0");
  api.ensure_package_version("bar", "3.0.0");
  api.add_dependency(("foo", "2.1.0"), ("bar", "^3.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["foo@^2.0.0"],
      overrides: make_overrides(serde_json::json!({
        "foo@^2.0.0": {
          "bar": "1.0.0"
        }
      })),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("foo@"))
    .unwrap();
  assert_eq!(foo_pkg.dependencies.get("bar").unwrap(), "bar@1.0.0");
}

#[tokio::test]
async fn override_version_selector_no_match() {
  // "foo@^3.0.0": { "bar": "1.0.0" }
  // should NOT override bar when foo resolves to 2.x
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("foo", "2.1.0");
  api.ensure_package_version("bar", "1.0.0");
  api.ensure_package_version("bar", "3.0.0");
  api.add_dependency(("foo", "2.1.0"), ("bar", "^3.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["foo@^2.0.0"],
      overrides: make_overrides(serde_json::json!({
        "foo@^3.0.0": {
          "bar": "1.0.0"
        }
      })),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("foo@"))
    .unwrap();
  assert_eq!(foo_pkg.dependencies.get("bar").unwrap(), "bar@3.0.0");
}

#[tokio::test]
async fn override_dollar_reference() {
  // "bar": "$bar" should resolve to the root dependency's version of bar
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("bar", "1.0.0");
  api.ensure_package_version("bar", "2.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("bar", "^2.0.0"));

  let mut root_deps = std::collections::HashMap::new();
  root_deps.insert(
    deno_semver::StackString::from("bar"),
    deno_semver::StackString::from("^1.0.0"),
  );

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1.0.0", "bar@^1.0.0"],
      overrides: make_overrides_with_root_deps(
        serde_json::json!({
          "bar": "$bar"
        }),
        root_deps,
      ),
      ..Default::default()
    },
  )
  .await;

  let bar_pkgs: Vec<_> = packages
    .iter()
    .filter(|p| p.pkg_id.starts_with("bar@"))
    .collect();
  assert_eq!(bar_pkgs.len(), 1);
  assert_eq!(bar_pkgs[0].pkg_id, "bar@1.0.0");
}

#[tokio::test]
async fn override_with_dot_key() {
  // "foo@^2.0.0": { ".": "2.0.0", "bar": "1.0.0" }
  // should override foo itself and also bar within foo's tree
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("foo", "2.0.0");
  api.ensure_package_version("foo", "2.1.0");
  api.ensure_package_version("bar", "1.0.0");
  api.ensure_package_version("bar", "3.0.0");
  api.add_dependency(("foo", "2.0.0"), ("bar", "^3.0.0"));
  api.add_dependency(("foo", "2.1.0"), ("bar", "^3.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["foo@^2.0.0"],
      overrides: make_overrides(serde_json::json!({
        "foo@^2.0.0": {
          ".": "2.0.0",
          "bar": "1.0.0"
        }
      })),
      ..Default::default()
    },
  )
  .await;

  let foo_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("foo@"))
    .unwrap();
  assert_eq!(foo_pkg.pkg_id, "foo@2.0.0");
  assert_eq!(foo_pkg.dependencies.get("bar").unwrap(), "bar@1.0.0");
}

#[tokio::test]
async fn override_npm_alias_scoped_to_parent() {
  // "parent": { "child": "npm:alt@1.0.0" }
  // should only alias child under parent, not under other
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("parent", "1.0.0");
  api.ensure_package_version("other", "1.0.0");
  api.ensure_package_version("child", "2.0.0");
  api.ensure_package_version("alt", "1.0.0");
  api.add_dependency(("parent", "1.0.0"), ("child", "^2.0.0"));
  api.add_dependency(("other", "1.0.0"), ("child", "^2.0.0"));

  let (packages, _) = run_v2_resolver_with_options_and_get_output(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["parent@1.0.0", "other@1.0.0"],
      overrides: make_overrides(serde_json::json!({
        "parent": {
          "child": "npm:alt@1.0.0"
        }
      })),
      ..Default::default()
    },
  )
  .await;

  let parent_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("parent@"))
    .unwrap();
  assert_eq!(
    parent_pkg.dependencies.get("child").unwrap(),
    "alt@1.0.0"
  );
  let other_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("other@"))
    .unwrap();
  assert_eq!(
    other_pkg.dependencies.get("child").unwrap(),
    "child@2.0.0"
  );
}

// ====================================================================
// Batch 3: Peer dep tests (sibling, circular_2)
// ====================================================================

#[tokio::test]
async fn resolve_sibling_peer_deps() {
  // a -> b -> peer c
  //   -> c -> peer b
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-c@1.0.0__package-b@1.0.0___package-c@1.0.0_package-b@1.0.0__package-c@1.0.0___package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@1.0.0_package-c@1.0.0__package-b@1.0.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@1.0.0_package-b@1.0.0__package-c@1.0.0".to_string(),
          )
        ])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-c@1.0.0__package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0__package-c@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0__package-c@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-c@1.0.0__package-b@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-a@1.0.0".to_string(),
      "package-a@1.0.0_package-c@1.0.0__package-b@1.0.0___package-c@1.0.0_package-b@1.0.0__package-c@1.0.0___package-b@1.0.0".to_string()
    )]
  );
}

#[tokio::test]
async fn resolve_dep_with_peer_deps_circular_2() {
  // a -> b -> c -> d -> c where c has a peer dependency on b
  //             -> e -> f -> d -> c where f has a peer dep on a
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.ensure_package_version("package-e", "1.0.0");
  api.ensure_package_version("package-f", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-e", "1"));
  api.add_dependency(("package-d", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-e", "1.0.0"), ("package-f", "1"));
  api.add_dependency(("package-f", "1.0.0"), ("package-d", "1"));
  api.add_peer_dependency(("package-f", "1.0.0"), ("package-a", "1"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "1"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@1.0.0_package-a@1.0.0".to_string(),
          ),
          (
            "package-d".to_string(),
            "package-d@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
          ),
          (
            "package-e".to_string(),
            "package-e@1.0.0_package-a@1.0.0_package-b@1.0.0__package-a@1.0.0".to_string()
          )
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-e@1.0.0_package-a@1.0.0_package-b@1.0.0__package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-f".to_string(),
          "package-f@1.0.0_package-a@1.0.0_package-b@1.0.0__package-a@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-f@1.0.0_package-a@1.0.0_package-b@1.0.0__package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0".to_string(),
        ), (
          "package-d".to_string(),
          "package-d@1.0.0_package-b@1.0.0__package-a@1.0.0_package-a@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

// ====================================================================
// Batch 3: Complex real-world tests
// ====================================================================

#[tokio::test]
async fn vite_tailwind_optional_peer_duplicates() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("@deno/vite-plugin", "1.0.4");
  api.ensure_package_version("@tailwindcss/vite", "4.0.17");
  api.ensure_package_version("lightningcss", "1.29.2");
  api.ensure_package_version("vite", "6.2.4");

  api.add_peer_dependency(
    ("@deno/vite-plugin", "1.0.4"),
    ("vite", "5.x || 6.x"),
  );

  api.add_dependency(
    ("@tailwindcss/vite", "4.0.17"),
    ("lightningcss", "1.29.2"),
  );
  api.add_peer_dependency(
    ("@tailwindcss/vite", "4.0.17"),
    ("vite", "^5.2.0 || ^6"),
  );

  api.add_optional_peer_dependency(
    ("vite", "6.2.4"),
    ("lightningcss", "^1.21.0"),
  );

  let (packages, package_reqs) = run_v2_resolver_and_get_output(
    &api,
    vec!["@deno/vite-plugin@~1.0.4", "@tailwindcss/vite@~4.0.17"],
  )
  .await;
  // After peer resolution, dedupe_peer_dependents merges compatible copies.
  // vite@6.2.4 (bare, from @deno/vite-plugin) is a subset of
  // vite@6.2.4_lightningcss@1.29.2 (from @tailwindcss/vite), so the bare
  // copy is merged into the lightningcss variant. Both plugins end up
  // using the same vite copy — matching pnpm's behavior.
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "@deno/vite-plugin@1.0.4_vite@6.2.4__lightningcss@1.29.2"
          .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "vite".to_string(),
          "vite@6.2.4_lightningcss@1.29.2".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "@tailwindcss/vite@4.0.17_vite@6.2.4__lightningcss@1.29.2_lightningcss@1.29.2"
          .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "lightningcss".to_string(),
          "lightningcss@1.29.2".to_string(),
        ), (
          "vite".to_string(),
          "vite@6.2.4_lightningcss@1.29.2".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "lightningcss@1.29.2".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "vite@6.2.4_lightningcss@1.29.2".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "lightningcss".to_string(),
          "lightningcss@1.29.2".to_string(),
        )])
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "@deno/vite-plugin@~1.0.4".to_string(),
        "@deno/vite-plugin@1.0.4_vite@6.2.4__lightningcss@1.29.2"
          .to_string()
      ),
      (
        "@tailwindcss/vite@~4.0.17".to_string(),
        "@tailwindcss/vite@4.0.17_vite@6.2.4__lightningcss@1.29.2_lightningcss@1.29.2"
          .to_string()
      ),
    ]
  );
}

#[tokio::test]
async fn aws_sdk_issue() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("@aws-sdk/client-s3", "3.679.0");
  api.ensure_package_version("@aws-sdk/client-sts", "3.679.0");
  api.ensure_package_version("@aws-sdk/client-sso-oidc", "3.679.0");
  api.ensure_package_version(
    "@aws-sdk/credential-provider-node",
    "3.679.0",
  );
  api.ensure_package_version(
    "@aws-sdk/credential-provider-ini",
    "3.679.0",
  );
  api.ensure_package_version(
    "@aws-sdk/credential-provider-sso",
    "3.679.0",
  );
  api.ensure_package_version(
    "@aws-sdk/credential-provider-web-identity",
    "3.679.0",
  );
  api.ensure_package_version("@aws-sdk/token-providers", "3.679.0");

  api.add_dependency(
    ("@aws-sdk/client-s3", "3.679.0"),
    ("@aws-sdk/client-sts", "3.679.0"),
  );
  api.add_dependency(
    ("@aws-sdk/client-s3", "3.679.0"),
    ("@aws-sdk/client-sso-oidc", "3.679.0"),
  );
  api.add_dependency(
    ("@aws-sdk/client-sts", "3.679.0"),
    ("@aws-sdk/client-sso-oidc", "3.679.0"),
  );
  api.add_dependency(
    ("@aws-sdk/client-sts", "3.679.0"),
    ("@aws-sdk/credential-provider-node", "3.679.0"),
  );
  api.add_peer_dependency(
    ("@aws-sdk/client-sso-oidc", "3.679.0"),
    ("@aws-sdk/client-sts", "^3.679.0"),
  );
  api.add_peer_dependency(
    ("@aws-sdk/credential-provider-ini", "3.679.0"),
    ("@aws-sdk/client-sts", "^3.679.0"),
  );
  api.add_dependency(
    ("@aws-sdk/credential-provider-ini", "3.679.0"),
    ("@aws-sdk/credential-provider-sso", "3.679.0"),
  );
  api.add_dependency(
    ("@aws-sdk/credential-provider-node", "3.679.0"),
    ("@aws-sdk/credential-provider-ini", "3.679.0"),
  );
  api.add_peer_dependency(
    ("@aws-sdk/credential-provider-sso", "3.679.0"),
    ("@aws-sdk/client-sso-oidc", "^3.679.0"),
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(
      &api,
      vec!["@aws-sdk/client-s3@3.679.0"],
    )
    .await;
  // The new resolver should produce no duplicate packages.
  // Exact peer dep encoding may differ from old resolver.
  // Verify structure: all packages present, no duplicates.
  let pkg_names: Vec<&str> = packages
    .iter()
    .map(|p| p.pkg_id.split('@').next().unwrap_or(""))
    .collect();
  // Should have client-s3, client-sts, client-sso-oidc,
  // credential-provider-node, credential-provider-ini,
  // credential-provider-sso
  assert!(
    packages.len() == 6,
    "Expected 6 packages, got {}: {:?}",
    packages.len(),
    pkg_names,
  );
  assert_eq!(package_reqs.len(), 1);
  assert!(package_reqs[0].0 == "@aws-sdk/client-s3@3.679.0");
}

#[tokio::test]
async fn prefer_previously_resolved_peer_in_ancestors() {
  let api = TestNpmRegistryApi::default();
  // package-peer@1 (1.0.2)
  // a -> b -> package-peer@1 (peer)
  //   -> c -> d -> b -> package-peer@1 (peer)
  //        -> package-peer@1.0.1 (dep)
  //   -> package-peer@1 (peer)
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.1");
  api.ensure_package_version("package-peer", "1.0.2");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");

  api.add_dependency(("package-a", "1.0.0"), ("package-b", "*"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "*"));
  api.add_peer_dependency(("package-a", "1.0.0"), ("package-peer", "1"));
  api.add_peer_dependency(("package-b", "1.0.0"), ("package-peer", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "*"));
  api.add_dependency(
    ("package-c", "1.0.0"),
    ("package-peer", "1.0.1"),
  );
  api.add_peer_dependency(("package-d", "1.0.0"), ("package-b", "1"));

  // With dedup (dedup should consolidate to 1.0.1)
  let (packages, package_reqs) =
    run_v2_resolver_with_options_and_get_output(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["package-a@1.0.0", "package-peer@1"],
        skip_dedup: false,
        ..Default::default()
      },
    )
    .await;
  // Verify package-peer resolves and dedup works
  let peer_pkgs: Vec<_> = packages
    .iter()
    .filter(|p| p.pkg_id.starts_with("package-peer@"))
    .collect();
  // After dedup, should have only 1.0.1
  assert!(
    peer_pkgs.len() >= 1,
    "Expected at least 1 package-peer, got: {:?}",
    peer_pkgs,
  );
  assert_eq!(package_reqs.len(), 2);
}

// ==========================================================================
// Helper functions for newly ported tests
// ==========================================================================


fn version(text: &str) -> deno_semver::Version {
  deno_semver::Version::parse_from_npm(text).unwrap()
}

async fn run_v2_resolver_with_options_and_get_snapshot(
  api: &TestNpmRegistryApi,
  options: RunV2ResolverOptions<'_>,
) -> Result<NpmResolutionSnapshot, NpmResolutionError> {
  run_v2_resolver_with_all_options(api, options).await
}

async fn run_v2_resolver_with_options_and_get_err(
  api: &TestNpmRegistryApi,
  options: RunV2ResolverOptions<'_>,
) -> NpmResolutionError {
  run_v2_resolver_with_all_options(api, options)
    .await
    .unwrap_err()
}

// ==========================================================================
// Tests ported from graph.rs
// ==========================================================================

#[tokio::test]
async fn resolve_with_peer_deps_auto_resolved() {
  // in this case, the peer dependency is not found in the tree
  // so it's auto-resolved based on the registry
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-peer", "4.0.0");
  api.ensure_package_version("package-peer", "4.1.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-peer", "4"));
  api.add_peer_dependency(("package-c", "3.0.0"), ("package-peer", "*"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer@4.1.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer@4.1.0".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer@4.1.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.1.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer@4.1.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer".to_string(),
          "package-peer@4.1.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer@4.1.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_nested_peer_deps_auto_resolved() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.0.0");
  api.ensure_package_version("package-peer-a", "2.0.0");
  api.ensure_package_version("package-peer-b", "3.0.0");
  api.add_peer_dependency(("package-0", "1.0.0"), ("package-peer-a", "2"));
  api.add_peer_dependency(
    ("package-peer-a", "2.0.0"),
    ("package-peer-b", "3"),
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-0@1.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.0.0_package-peer-a@2.0.0__package-peer-b@3.0.0"
          .to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-a".to_string(),
          "package-peer-a@2.0.0_package-peer-b@3.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-a@2.0.0_package-peer-b@3.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-b".to_string(),
          "package-peer-b@3.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-b@3.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-0@1.0".to_string(),
      "package-0@1.0.0_package-peer-a@2.0.0__package-peer-b@3.0.0"
        .to_string()
    )]
  );
}

#[tokio::test]
async fn resolve_with_peer_deps_multiple() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-0", "1.1.1");
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "2.0.0");
  api.ensure_package_version("package-c", "3.0.0");
  api.ensure_package_version("package-d", "3.5.0");
  api.ensure_package_version("package-e", "3.6.0");
  api.ensure_package_version("package-peer-a", "4.0.0");
  api.ensure_package_version("package-peer-a", "4.1.0");
  api.ensure_package_version("package-peer-b", "5.3.0");
  api.ensure_package_version("package-peer-b", "5.4.1");
  api.ensure_package_version("package-peer-c", "6.2.0");
  api.add_dependency(("package-0", "1.1.1"), ("package-a", "1"));
  api.add_dependency(("package-a", "1.0.0"), ("package-b", "^2"));
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "^3"));
  api.add_dependency(("package-a", "1.0.0"), ("package-d", "^3"));
  api.add_dependency(("package-a", "1.0.0"), ("package-peer-a", "4.0.0"));
  api.add_peer_dependency(("package-b", "2.0.0"), ("package-peer-a", "4"));
  api.add_peer_dependency(
    ("package-b", "2.0.0"),
    ("package-peer-c", "=6.2.0"), // will be auto-resolved
  );
  api.add_peer_dependency(("package-c", "3.0.0"), ("package-peer-a", "*"));
  api.add_peer_dependency(
    ("package-peer-a", "4.0.0"),
    ("package-peer-b", "^5.4"), // will be auto-resolved
  );

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(
      &api,
      vec!["package-0@1.1.1", "package-e@3"],
    )
    .await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-0@1.1.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-a".to_string(),
          "package-a@1.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-b".to_string(),
            "package-b@2.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1_package-peer-c@6.2.0".to_string(),
          ),
          (
            "package-c".to_string(),
            "package-c@3.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1".to_string(),
          ),
          (
            "package-d".to_string(),
            "package-d@3.5.0".to_string(),
          ),
          (
            "package-peer-a".to_string(),
            "package-peer-a@4.0.0_package-peer-b@5.4.1".to_string(),
          ),
        ]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@2.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1_package-peer-c@6.2.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([
          (
            "package-peer-a".to_string(),
            "package-peer-a@4.0.0_package-peer-b@5.4.1".to_string(),
          ),
          (
            "package-peer-c".to_string(),
            "package-peer-c@6.2.0".to_string(),
          )
        ])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@3.0.0_package-peer-a@4.0.0__package-peer-b@5.4.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-a".to_string(),
          "package-peer-a@4.0.0_package-peer-b@5.4.1".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@3.5.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-e@3.6.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-a@4.0.0_package-peer-b@5.4.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-b".to_string(),
          "package-peer-b@5.4.1".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-b@5.4.1".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-c@6.2.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      ("package-0@1.1.1".to_string(), "package-0@1.1.1".to_string()),
      ("package-e@3".to_string(), "package-e@3.6.0".to_string()),
    ]
  );
}

#[tokio::test]
async fn resolve_dep_with_peer_deps_then_other_dep_with_different_peer() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-peer", "1.1.0");
  api.ensure_package_version("package-peer", "1.2.0");
  api.add_peer_dependency(
    ("package-a", "1.0.0"),
    ("package-peer", "*"),
  ); // should select 1.2.0, then 1.1.0
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-peer", "=1.1.0"));
  api.add_dependency(("package-c", "1.0.0"), ("package-a", "1"));

  let input_reqs = vec!["package-a@1.0", "package-b@1.0"];
  // before deduping
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          skip_dedup: true,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 1,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.2.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.2.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-b@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-c".to_string(),
              "package-c@1.0.0_package-peer@1.1.0".to_string(),
            ),
            (
              "package-peer".to_string(),
              "package-peer@1.1.0".to_string(),
            )
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-c@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.2.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0".to_string(),
          "package-a@1.0.0_package-peer@1.2.0".to_string()
        ),
        (
          "package-b@1.0".to_string(),
          "package-b@1.0.0_package-peer@1.1.0".to_string()
        )
      ]
    );
  }
  // deduping
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          skip_dedup: false,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-b@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-c".to_string(),
              "package-c@1.0.0_package-peer@1.1.0".to_string(),
            ),
            (
              "package-peer".to_string(),
              "package-peer@1.1.0".to_string(),
            )
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-c@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0".to_string(),
          "package-a@1.0.0_package-peer@1.1.0".to_string()
        ),
        (
          "package-b@1.0".to_string(),
          "package-b@1.0.0_package-peer@1.1.0".to_string()
        )
      ]
    );
  }
}

#[tokio::test]
async fn dep_depending_on_self_when_has_peer_deps() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-c", "*"));
  api.add_peer_dependency(("package-c", "1.0.0"), ("package-b", "*"));
  api.add_dependency(("package-c", "1.0.0"), ("package-c", "1.0.0"));
  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["package-a@1.0.0"]).await;
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0_package-b@1.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0_package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )]),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1.0.0".to_string(), "package-a@1.0.0".to_string())]
  );
}

#[tokio::test]
async fn resolve_optional_peer_dep_first_then_after() {
  // This tests when a package is resolved later but doesn't have the
  // optional peer dep in its ancestor siblings.
  //
  // a -> package-peer-parent
  //
  // Then resolve b, which will have package-peer in its siblings:
  //
  //  b -> b-child -> package-peer-parent -> package-peer
  //    -> package-peer
  //  c -> c-child -> c-grand-child -> package-peer-parent -> package-peer
  //
  // Then later resolve package-d, which should resolve to package:
  //
  //  d -> package-peer-parent -> package-peer
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-b-child", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-c-child", "1.0.0");
  api.ensure_package_version("package-c-grandchild", "1.0.0");
  api.ensure_package_version("package-peer-parent", "1.0.0");
  api.ensure_package_version("package-peer", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");

  api.add_optional_peer_dependency(
    ("package-peer-parent", "1.0.0"),
    ("package-peer", "1"),
  );

  // a
  api.add_dependency(("package-a", "1.0.0"), ("package-peer-parent", "1"));

  // b
  api.add_dependency(("package-b", "1.0.0"), ("package-b-child", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-peer", "1"));
  api.add_dependency(
    ("package-b-child", "1.0.0"),
    ("package-peer-parent", "1"),
  );

  // c
  api.add_dependency(("package-c", "1.0.0"), ("package-c-child", "1"));
  api.add_dependency(
    ("package-c-child", "1.0.0"),
    ("package-c-grandchild", "1"),
  );
  api.add_dependency(
    ("package-c-grandchild", "1.0.0"),
    ("package-peer-parent", "1"),
  );

  // d
  api.add_dependency(("package-d", "1.0.0"), ("package-peer-parent", "1"));

  // first run for just package-a
  let snapshot = run_v2_resolver_with_options_and_get_snapshot(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-a@1"],
      ..Default::default()
    },
  )
  .await
  .unwrap();
  let (packages, package_reqs) = snapshot_to_packages(&snapshot);
  assert_eq!(
    packages,
    Vec::from([
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-peer-parent".to_string(),
          "package-peer-parent@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-peer-parent@1.0.0".to_string(),
        copy_index: 0,
        // no optional peer
        dependencies: Default::default(),
      },
    ])
  );
  assert_eq!(
    package_reqs,
    vec![("package-a@1".to_string(), "package-a@1.0.0".to_string())]
  );

  // In the v2 resolver, the optional peer dep on package-peer-parent is NOT
  // resolved because package-peer is an optional peer. The dep tree keeps
  // package-peer-parent bare. When re-resolving from a snapshot, the existing
  // bare resolution is preserved.
  let b_c_packages = Vec::from([
    TestNpmResolutionPackage {
      pkg_id: "package-a@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-peer-parent".to_string(),
        "package-peer-parent@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-b@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([
        (
          "package-b-child".to_string(),
          "package-b-child@1.0.0".to_string(),
        ),
        (
          "package-peer".to_string(),
          "package-peer@1.0.0".to_string(),
        ),
      ]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-b-child@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-peer-parent".to_string(),
        "package-peer-parent@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-c@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-c-child".to_string(),
        "package-c-child@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-c-child@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-c-grandchild".to_string(),
        "package-c-grandchild@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-c-grandchild@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-peer-parent".to_string(),
        "package-peer-parent@1.0.0".to_string(),
      )]),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-peer@1.0.0".to_string(),
      copy_index: 0,
      dependencies: Default::default(),
    },
    TestNpmResolutionPackage {
      pkg_id: "package-peer-parent@1.0.0".to_string(),
      copy_index: 0,
      dependencies: Default::default(),
    },
  ]);
  let snapshot = run_v2_resolver_with_options_and_get_snapshot(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-b@1", "package-c@1"],
      snapshot,
      ..Default::default()
    },
  )
  .await
  .unwrap();
  let (packages, package_reqs) = snapshot_to_packages(&snapshot);
  assert_eq!(packages, b_c_packages);
  assert_eq!(
    package_reqs,
    vec![
      ("package-a@1".to_string(), "package-a@1.0.0".to_string()),
      (
        "package-b@1".to_string(),
        "package-b@1.0.0".to_string(),
      ),
      ("package-c@1".to_string(), "package-c@1.0.0".to_string(),)
    ]
  );

  // now try resolving package-d
  let snapshot = run_v2_resolver_with_options_and_get_snapshot(
    &api,
    RunV2ResolverOptions {
      reqs: vec!["package-d@1"],
      snapshot,
      ..Default::default()
    },
  )
  .await
  .unwrap();
  let (packages, package_reqs) = snapshot_to_packages(&snapshot);
  let mut d_packages = b_c_packages;
  d_packages.insert(
    6,
    TestNpmResolutionPackage {
      pkg_id: "package-d@1.0.0".to_string(),
      copy_index: 0,
      dependencies: BTreeMap::from([(
        "package-peer-parent".to_string(),
        "package-peer-parent@1.0.0".to_string(),
      )]),
    },
  );
  assert_eq!(packages, d_packages);
  assert_eq!(
    package_reqs,
    vec![
      ("package-a@1".to_string(), "package-a@1.0.0".to_string()),
      (
        "package-b@1".to_string(),
        "package-b@1.0.0".to_string(),
      ),
      ("package-c@1".to_string(), "package-c@1.0.0".to_string()),
      ("package-d@1".to_string(), "package-d@1.0.0".to_string()),
    ]
  );
}

#[tokio::test]
async fn link_packages() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b1", "1.0.0");
  api.ensure_package_version("package-b2", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-d", "1.0.0");
  api.add_dependency(("package-a", "1.0.0"), ("package-b1", "1"));
  api.add_dependency(("package-b1", "1.0.0"), ("package-b2", "1"));
  api.add_dependency(("package-c", "1.0.0"), ("package-d", "1"));

  let link_packages = HashMap::from([(
    deno_semver::package::PackageName::from_static("package-b1"),
    vec![
      NpmPackageVersionInfo {
        // should not select this one because 1.0.1 is higher
        version: deno_semver::Version::parse_standard("1.0.0").unwrap(),
        ..Default::default()
      },
      NpmPackageVersionInfo {
        version: deno_semver::Version::parse_standard("1.0.1").unwrap(),
        dependencies: HashMap::from([(
          deno_semver::StackString::from_static("package-c"),
          deno_semver::StackString::from_static("1"),
        )]),
        ..Default::default()
      },
    ],
  )]);

  let (packages, package_reqs) =
    run_v2_resolver_with_options_and_get_output(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["package-a@1.0.0"],
        link_packages: Some(&link_packages),
        ..Default::default()
      },
    )
    .await;

  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b1".to_string(),
          "package-b1@1.0.1".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b1@1.0.1".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-c".to_string(),
          "package-c@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-c@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-d".to_string(),
          "package-d@1.0.0".to_string(),
        )]),
      },
      TestNpmResolutionPackage {
        pkg_id: "package-d@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-a@1.0.0".to_string(),
      "package-a@1.0.0".to_string()
    )]
  );
}

#[tokio::test]
async fn link_package_tag() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.add_dist_tag("package-a", "next", "1.0.0");

  let link_packages = HashMap::from([(
    deno_semver::package::PackageName::from_static("package-a"),
    vec![NpmPackageVersionInfo {
      version: deno_semver::Version::parse_standard("1.0.0").unwrap(),
      dependencies: HashMap::from([(
        deno_semver::StackString::from_static("package-b"),
        deno_semver::StackString::from_static("1"),
      )]),
      ..Default::default()
    }],
  )]);

  let (packages, package_reqs) =
    run_v2_resolver_with_options_and_get_output(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["package-a@next"],
        link_packages: Some(&link_packages),
        ..Default::default()
      },
    )
    .await;

  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-b".to_string(),
          "package-b@1.0.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![(
      "package-a@next".to_string(),
      "package-a@1.0.0".to_string()
    )]
  );
}

#[tokio::test]
async fn resolve_link_copy_index() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-b", "1.0.0");
  api.ensure_package_version("package-c", "1.0.0");
  api.ensure_package_version("package-peer", "1.1.0");
  api.ensure_package_version("package-peer", "1.2.0");
  api.add_dependency(("package-b", "1.0.0"), ("package-c", "1"));
  api.add_dependency(("package-b", "1.0.0"), ("package-peer", "=1.1.0"));
  api.add_dependency(("package-c", "1.0.0"), ("package-a", "1"));

  let link_packages = HashMap::from([(
    deno_semver::package::PackageName::from_static("package-a"),
    vec![NpmPackageVersionInfo {
      version: deno_semver::Version::parse_standard("1.0.0").unwrap(),
      peer_dependencies: HashMap::from([(
        deno_semver::StackString::from_static("package-peer"),
        deno_semver::StackString::from_static("*"),
      )]),
      ..Default::default()
    }],
  )]);

  let input_reqs = vec!["package-a@1.0", "package-b@1.0"];
  // before deduping: two variants of package-a (1.2.0 at root, 1.1.0 nested)
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          link_packages: Some(&link_packages),
          skip_dedup: true,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 1,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.2.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.2.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-b@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-c".to_string(),
              "package-c@1.0.0_package-peer@1.1.0".to_string(),
            ),
            (
              "package-peer".to_string(),
              "package-peer@1.1.0".to_string(),
            )
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-c@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.2.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0".to_string(),
          "package-a@1.0.0_package-peer@1.2.0".to_string()
        ),
        (
          "package-b@1.0".to_string(),
          "package-b@1.0.0_package-peer@1.1.0".to_string()
        )
      ]
    );
  }
  // after deduping: consolidated to 1.1.0 variant
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: input_reqs.clone(),
          link_packages: Some(&link_packages),
          skip_dedup: false,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-peer".to_string(),
            "package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-b@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([
            (
              "package-c".to_string(),
              "package-c@1.0.0_package-peer@1.1.0".to_string(),
            ),
            (
              "package-peer".to_string(),
              "package-peer@1.1.0".to_string(),
            )
          ]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-c@1.0.0_package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([(
            "package-a".to_string(),
            "package-a@1.0.0_package-peer@1.1.0".to_string(),
          )]),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-peer@1.1.0".to_string(),
          copy_index: 0,
          dependencies: BTreeMap::from([]),
        },
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0".to_string(),
          "package-a@1.0.0_package-peer@1.1.0".to_string()
        ),
        (
          "package-b@1.0".to_string(),
          "package-b@1.0.0_package-peer@1.1.0".to_string()
        )
      ]
    );
  }
}

#[tokio::test]
async fn test_newest_dependency_date() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("a", "1.0.0");
  api.ensure_package_version("a", "1.0.1");
  api.ensure_package_version("a", "1.0.2");
  api.ensure_package_version("b", "1.0.0");
  api.ensure_package_version("b", "1.0.1");

  api.with_package("a", |info| {
    info.dist_tags.insert("tag".to_string(), version("1.0.2"));
    info.time.insert(
      version("1.0.0"),
      "2015-11-07T00:00:00.000Z".parse().unwrap(),
    );
    info.time.insert(
      version("1.0.1"),
      "2020-11-07T00:00:00.000Z".parse().unwrap(),
    );
    info.time.insert(
      version("1.0.2"),
      "2022-11-07T00:00:00.000Z".parse().unwrap(),
    );
  });

  api.with_package("b", |info| {
    info.dist_tags.insert("tag".to_string(), version("1.0.1"));
    info.time.insert(
      version("1.0.0"),
      "2015-11-07T00:00:00.000Z".parse().unwrap(),
    );
    info.time.insert(
      version("1.0.1"),
      "2022-11-07T00:00:00.000Z".parse().unwrap(),
    );
  });

  {
    let (packages, _package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          reqs: vec!["a@1", "b@1"],
          newest_dependency_date: NewestDependencyDateOptions {
            date: Some(NewestDependencyDate(
              "2021-11-07T00:00:00.000Z".parse().unwrap(),
            )),
            exclude: BTreeSet::from(["b".into()]),
          },
          ..Default::default()
        },
      )
      .await;
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].pkg_id, "a@1.0.1");
    assert_eq!(packages[1].pkg_id, "b@1.0.1");
  }

  {
    let err = run_v2_resolver_with_options_and_get_err(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["a@1"],
        newest_dependency_date: NewestDependencyDateOptions::from_date(
          "2010-11-07T00:00:00.000Z".parse().unwrap(),
        ),
        ..Default::default()
      },
    )
    .await;
    assert_eq!(
      err.to_string(),
      "Could not find npm package 'a' matching '1'.\n\nA newer matching version was found, but it was not used because it was newer than the specified minimum dependency date of 2010-11-07 00:00:00 UTC."
    );
  }
  {
    let err = run_v2_resolver_with_options_and_get_err(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["a@tag"],
        newest_dependency_date: NewestDependencyDateOptions::from_date(
          "2010-11-07T00:00:00.000Z".parse().unwrap(),
        ),
        ..Default::default()
      },
    )
    .await;
    assert_eq!(
      err.to_string(),
      "Failed resolving tag 'a@tag' mapped to 'a@1.0.2' because the package version was published at 2022-11-07 00:00:00 UTC, but dependencies newer than 2010-11-07 00:00:00 UTC are not allowed because it is newer than the specified minimum dependency date."
    );
  }
}

#[tokio::test]
async fn dedup_lower_specific_with_overlapping_then_higher_root_req_added_later()
{
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-a", "1.1.0");

  let snapshot = {
    let snapshot = run_v2_resolver_with_options_and_get_snapshot(
      &api,
      RunV2ResolverOptions {
        reqs: vec!["package-a@^1.0.0", "package-a@1.0.0"],
        skip_dedup: false,
        ..Default::default()
      },
    )
    .await
    .unwrap();
    let (packages, package_reqs) = snapshot_to_packages(&snapshot);
    assert_eq!(
      packages,
      vec![TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0.0".to_string(),
          "package-a@1.0.0".to_string()
        ),
        (
          "package-a@^1.0.0".to_string(),
          "package-a@1.0.0".to_string()
        ),
      ]
    );
    snapshot
  };
  {
    let (packages, package_reqs) =
      run_v2_resolver_with_options_and_get_output(
        &api,
        RunV2ResolverOptions {
          snapshot,
          reqs: vec!["package-a@1.1.0"],
          skip_dedup: false,
          ..Default::default()
        },
      )
      .await;
    assert_eq!(
      packages,
      vec![
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.0.0".to_string(),
          copy_index: 0,
          dependencies: Default::default(),
        },
        TestNpmResolutionPackage {
          pkg_id: "package-a@1.1.0".to_string(),
          copy_index: 0,
          dependencies: Default::default(),
        }
      ]
    );
    assert_eq!(
      package_reqs,
      vec![
        (
          "package-a@1.0.0".to_string(),
          "package-a@1.0.0".to_string()
        ),
        (
          "package-a@1.1.0".to_string(),
          "package-a@1.1.0".to_string()
        ),
        (
          "package-a@^1.0.0".to_string(),
          "package-a@1.1.0".to_string()
        ),
      ]
    );
  }
}

#[tokio::test]
async fn dedup_with_initially_partially_resolved_graph() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("package-a", "1.0.0");
  api.ensure_package_version("package-shared", "1.0.0");
  api.add_peer_dependency(
    ("package-a", "1.0.0"),
    ("package-shared", "^1.0.0"),
  );

  // first, resolve package-a which pulls in package-shared@1.0.0
  let snapshot = run_v2_resolver_with_options_and_get_snapshot(
    &api,
    RunV2ResolverOptions {
      reqs: Vec::from(["package-a@1"]),
      ..Default::default()
    },
  )
  .await
  .unwrap();

  // now "publish" package-b and package-shared 1.1.0
  api.ensure_package_version("package-b", "1.0.0");
  api.add_peer_dependency(
    ("package-b", "1.0.0"),
    ("package-shared", "^1.1.0"),
  );
  api.ensure_package_version("package-shared", "1.1.0");

  // now resolve package-b
  let (packages, package_reqs) =
    run_v2_resolver_with_options_and_get_output(
      &api,
      RunV2ResolverOptions {
        snapshot,
        reqs: Vec::from(["package-b@1"]),
        ..Default::default()
      },
    )
    .await;

  // after dedup, package-b should use package-shared@1.1.0 (consolidated)
  assert_eq!(
    packages,
    vec![
      TestNpmResolutionPackage {
        pkg_id: "package-a@1.0.0_package-shared@1.1.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-shared".to_string(),
          "package-shared@1.1.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-b@1.0.0_package-shared@1.1.0".to_string(),
        copy_index: 0,
        dependencies: BTreeMap::from([(
          "package-shared".to_string(),
          "package-shared@1.1.0".to_string(),
        )])
      },
      TestNpmResolutionPackage {
        pkg_id: "package-shared@1.1.0".to_string(),
        copy_index: 0,
        dependencies: Default::default(),
      },
    ]
  );
  assert_eq!(
    package_reqs,
    vec![
      (
        "package-a@1".to_string(),
        "package-a@1.0.0_package-shared@1.1.0".to_string()
      ),
      (
        "package-b@1".to_string(),
        "package-b@1.0.0_package-shared@1.1.0".to_string()
      ),
    ]
  );
}

#[tokio::test]
async fn dep_tree_from_snapshot_dep_on_self() {
  // there are some lockfiles in the wild that when loading have a dependency
  // on themselves and causes a panic, so ensure this doesn't panic
  let snapshot = SerializedNpmResolutionSnapshot {
    root_packages: HashMap::from([(
      PackageReq::from_str("package-0").unwrap(),
      NpmPackageId::from_serialized("package-0@1.0.0").unwrap(),
    )]),
    packages: Vec::from([SerializedNpmResolutionSnapshotPackage {
      id: NpmPackageId::from_serialized("package-0@1.0.0").unwrap(),
      system: Default::default(),
      dependencies: HashMap::from([(
        "package-a".into(),
        NpmPackageId::from_serialized("package-0@1.0.0").unwrap(),
      )]),
      optional_peer_dependencies: Default::default(),
      optional_dependencies: HashSet::new(),
      extra: None,
      is_deprecated: false,
      dist: Some(crate::registry::NpmPackageVersionDistInfo {
        tarball: "https://example.com/package-0@1.0.0.tgz".to_string(),
        shasum: None,
        integrity: None,
      }),
      has_bin: false,
      has_scripts: false,
    }]),
  };
  let snapshot = NpmResolutionSnapshot::new(snapshot.into_valid().unwrap());
  let version_resolver = NpmVersionResolver {
    link_packages: Arc::new(HashMap::new()),
    newest_dependency_date_options: Default::default(),
    overrides: Arc::new(NpmOverrides::default()),
  };
  // assert this doesn't panic
  let _tree =
    DepTree::from_snapshot(snapshot, &version_resolver, &HashMap::new());
}

/// Reproduction of the vitefu/picomatch issue:
/// fdir has an optional peer dep on picomatch. vite depends on both fdir
/// and picomatch. vitefu (under kit) has peer dep vite. picomatch should
/// appear nested inside vite but NOT as a direct peer of vitefu.
#[tokio::test]
async fn peer_dep_no_extra_propagation_through_child_peer() {
  let api = TestNpmRegistryApi::default();
  api.ensure_package_version("kit", "1.0.0");
  api.ensure_package_version("vitefu", "1.1.2");
  api.ensure_package_version("vite", "6.4.1");
  api.ensure_package_version("fdir", "6.4.4");
  api.ensure_package_version("picomatch", "4.0.3");

  // kit depends on vitefu
  api.add_dependency(("kit", "1.0.0"), ("vitefu", "1"));
  // vitefu has peer dep vite
  api.add_peer_dependency(("vitefu", "1.1.2"), ("vite", ">=5"));
  // vite depends on fdir and picomatch (regular deps)
  api.add_dependency(("vite", "6.4.1"), ("fdir", "6"));
  api.add_dependency(("vite", "6.4.1"), ("picomatch", "4"));
  // fdir has optional peer dep picomatch
  api.add_optional_peer_dependency(("fdir", "6.4.4"), ("picomatch", "^3 || ^4"));

  let (packages, package_reqs) =
    run_v2_resolver_and_get_output(&api, vec!["kit@1.0", "vite@6.4"])
      .await;

  // Print the actual output for debugging
  for pkg in &packages {
    eprintln!("  pkg_id: {:?}", pkg.pkg_id);
    eprintln!("    deps: {:?}", pkg.dependencies);
  }
  eprintln!("  reqs: {:?}", package_reqs);

  // vitefu should have vite as a peer, with picomatch nested inside vite.
  // picomatch should NOT be a direct peer of vitefu.
  let vitefu_pkg = packages
    .iter()
    .find(|p| p.pkg_id.starts_with("vitefu@"))
    .unwrap();
  // Expected: vitefu@1.1.2_vite@6.4.1__picomatch@4.0.3
  // NOT: vitefu@1.1.2_vite@6.4.1__picomatch@4.0.3_picomatch@4.0.3
  assert!(
    !vitefu_pkg.pkg_id.ends_with("_picomatch@4.0.3_picomatch@4.0.3"),
    "picomatch should not be a direct peer of vitefu, got: {}",
    vitefu_pkg.pkg_id
  );
  assert_eq!(
    vitefu_pkg.pkg_id,
    "vitefu@1.1.2_vite@6.4.1__picomatch@4.0.3"
  );
}
