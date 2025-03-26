// Copyright 2018-2025 the Deno authors. MIT license.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use deno_error::JsErrorBox;
use deno_npm::npm_rc::ResolvedNpmRc;
use deno_npm::registry::NpmPackageInfo;
use deno_npm::registry::NpmPackageVersionInfo;
use deno_npm::registry::NpmRegistryApi;
use deno_npm::registry::NpmRegistryPackageInfoLoadError;
use deno_npm::registry::SmallNpmPackageInfo;
use deno_semver::package::PackageNv;
use deno_semver::StackString;
use deno_semver::Version;
use deno_unsync::sync::AtomicFlag;
use deno_unsync::sync::MultiRuntimeAsyncValueCreator;
use futures::future::LocalBoxFuture;
use futures::FutureExt;
use parking_lot::Mutex;
use sys_traits::FsCreateDirAll;
use sys_traits::FsHardLink;
use sys_traits::FsMetadata;
use sys_traits::FsOpen;
use sys_traits::FsReadDir;
use sys_traits::FsRemoveFile;
use sys_traits::FsRename;
use sys_traits::SystemRandom;
use sys_traits::ThreadSleep;
use url::Url;

use crate::remote::maybe_auth_header_for_npm_registry;
use crate::NpmCache;
use crate::NpmCacheHttpClient;
use crate::NpmCacheSetting;

type LoadResult = Result<FutureResult, Arc<JsErrorBox>>;
type LoadFuture = LocalBoxFuture<'static, LoadResult>;

#[derive(Debug, Clone)]
pub struct CacheInfo {
  pub versions: Arc<deno_npm::registry::SmallNpmPackageInfo>,
  pub content: String,
  pub version_ranges: HashMap<String, (u32, u32)>,
}

impl CacheInfo {
  pub fn new(name: String, content: String) -> Self {
    let fast_registry_json::Versions {
      dist_tags,
      version_ranges,
      versions,
    } = fast_registry_json::pluck_versions(&content).unwrap();
    let info: Arc<SmallNpmPackageInfo> =
      Arc::new(deno_npm::registry::SmallNpmPackageInfo {
        name: StackString::from_string(name),
        versions: versions
          .iter()
          .map(|v| Version::parse_from_npm(v).unwrap())
          .collect(),
        dist_tags: dist_tags
          .into_iter()
          .map(|(k, v)| (k.to_string(), Version::parse_from_npm(v).unwrap()))
          .collect(),
      });

    let version_ranges = versions
      .iter()
      .zip(version_ranges.iter())
      .map(|(v, r)| (v.to_string(), r.clone()))
      .collect();

    Self {
      versions: info,
      content,
      version_ranges,
    }
  }
}

#[derive(Debug, Clone)]
enum FutureResult {
  PackageNotExists,
  SavedFsCache(Arc<CacheInfo>),
  ErroredFsCache(Arc<CacheInfo>),
}

#[derive(Debug, Clone)]
enum MemoryCacheItem {
  /// The cache item hasn't loaded yet.
  Pending(Arc<MultiRuntimeAsyncValueCreator<LoadResult>>),
  /// The item has loaded in the past and was stored in the file system cache.
  /// There is no reason to request this package from the npm registry again
  /// for the duration of execution.
  FsCached(Arc<CacheInfo>),
  /// An item is memory cached when it fails saving to the file system cache
  /// or the package does not exist.
  MemoryCached(Result<Option<Arc<CacheInfo>>, Arc<JsErrorBox>>),
}

#[derive(Debug, Default)]
struct MemoryCache {
  clear_id: usize,
  items: HashMap<String, MemoryCacheItem>,
  versions: HashMap<PackageNv, Arc<NpmPackageVersionInfo>>,
}

impl MemoryCache {
  #[inline(always)]
  pub fn clear(&mut self) {
    self.clear_id += 1;
    self.items.clear();
  }

  #[inline(always)]
  pub fn get(&self, key: &str) -> Option<&MemoryCacheItem> {
    self.items.get(key)
  }

  #[inline(always)]
  pub fn insert(&mut self, key: String, value: MemoryCacheItem) {
    self.items.insert(key, value);
  }

  #[inline(always)]
  pub fn try_insert(
    &mut self,
    clear_id: usize,
    key: &str,
    value: MemoryCacheItem,
  ) -> bool {
    if clear_id != self.clear_id {
      return false;
    }
    // if the clear_id is the same then the item should exist
    debug_assert!(self.items.contains_key(key));
    if let Some(item) = self.items.get_mut(key) {
      *item = value;
    }
    true
  }
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
#[class(generic)]
pub enum LoadFileCachedPackageInfoError {
  #[error("Previously saved '{name}' from the npm cache, but now it fails to load: {err}")]
  LoadPackageInfo {
    err: serde_json::Error,
    name: String,
  },
  #[error("The package '{0}' previously saved its registry information to the file system cache, but that file no longer exists.")]
  FileMissing(String),
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
#[class(inherit)]
#[error("Failed loading {url} for package \"{name}\"")]
pub struct LoadPackageInfoError {
  url: Url,
  name: String,
  #[inherit]
  #[source]
  inner: LoadPackageInfoInnerError,
}

#[derive(Debug, thiserror::Error, deno_error::JsError)]
pub enum LoadPackageInfoInnerError {
  #[class(inherit)]
  #[error("{0}")]
  LoadFileCachedPackageInfo(LoadFileCachedPackageInfoError),
  #[class(inherit)]
  #[error("{0}")]
  Other(Arc<JsErrorBox>),
}

// todo(#27198): refactor to store this only in the http cache

/// Downloads packuments from the npm registry.
///
/// This is shared amongst all the workers.
#[derive(Debug)]
pub struct RegistryInfoProvider<
  THttpClient: NpmCacheHttpClient,
  TSys: FsCreateDirAll
    + FsHardLink
    + FsMetadata
    + FsOpen
    + FsReadDir
    + FsRemoveFile
    + FsRename
    + ThreadSleep
    + SystemRandom
    + Send
    + Sync
    + 'static,
> {
  // todo(#27198): remove this
  cache: Arc<NpmCache<TSys>>,
  http_client: Arc<THttpClient>,
  npmrc: Arc<ResolvedNpmRc>,
  force_reload_flag: AtomicFlag,
  memory_cache: Mutex<MemoryCache>,
  previously_loaded_packages: Mutex<HashSet<String>>,
}

impl<
    THttpClient: NpmCacheHttpClient,
    TSys: FsCreateDirAll
      + FsHardLink
      + FsMetadata
      + FsOpen
      + FsReadDir
      + FsRemoveFile
      + FsRename
      + ThreadSleep
      + SystemRandom
      + Send
      + Sync
      + 'static,
  > RegistryInfoProvider<THttpClient, TSys>
{
  pub fn new(
    cache: Arc<NpmCache<TSys>>,
    http_client: Arc<THttpClient>,
    npmrc: Arc<ResolvedNpmRc>,
  ) -> Self {
    Self {
      cache,
      http_client,
      npmrc,
      force_reload_flag: AtomicFlag::lowered(),
      memory_cache: Default::default(),
      previously_loaded_packages: Default::default(),
    }
  }

  /// Clears the internal memory cache.
  pub fn clear_memory_cache(&self) {
    self.memory_cache.lock().clear();
  }

  fn mark_force_reload(&self) -> bool {
    // never force reload the registry information if reloading
    // is disabled or if we're already reloading
    if matches!(
      self.cache.cache_setting(),
      NpmCacheSetting::Only | NpmCacheSetting::ReloadAll
    ) {
      return false;
    }
    if self.force_reload_flag.raise() {
      self.clear_memory_cache();
      true
    } else {
      false
    }
  }

  pub fn as_npm_registry_api(
    self: &Arc<Self>,
  ) -> NpmRegistryApiAdapter<THttpClient, TSys> {
    NpmRegistryApiAdapter(self.clone())
  }

  pub async fn package_info(
    self: &Arc<Self>,
    name: &str,
  ) -> Result<Arc<SmallNpmPackageInfo>, NpmRegistryPackageInfoLoadError> {
    match self.maybe_package_info(name).await {
      Ok(Some(info)) => Ok(info),
      Ok(None) => Err(NpmRegistryPackageInfoLoadError::PackageNotExists {
        package_name: name.to_string(),
      }),
      Err(err) => Err(NpmRegistryPackageInfoLoadError::LoadError(Arc::new(
        JsErrorBox::from_err(err),
      ))),
    }
  }

  pub async fn maybe_package_version_info(
    self: &Arc<Self>,
    package_nv: &PackageNv,
  ) -> Result<Option<Arc<NpmPackageVersionInfo>>, LoadPackageInfoError> {
    if let Some(info) = self.memory_cache.lock().versions.get(package_nv) {
      return Ok(Some(info.clone()));
    }
    let Some(info) = self
      .load_package_info_inner(&package_nv.name)
      .await
      .map_err(|err| LoadPackageInfoError {
        url: get_package_url(&self.npmrc, &package_nv.name),
        name: package_nv.name.to_string(),
        inner: err,
      })?
    else {
      return Ok(None);
    };
    let &(start, end) = info
      .version_ranges
      .get(&package_nv.version.to_string())
      .unwrap();
    let parsed: NpmPackageVersionInfo =
      serde_json::from_str(&info.content[start as usize..end as usize])
        .map_err(|err| LoadPackageInfoError {
          url: get_package_url(&self.npmrc, &package_nv.name),
          name: package_nv.name.to_string(),
          inner: LoadPackageInfoInnerError::Other(Arc::new(
            JsErrorBox::from_err(err),
          )),
        })?;
    let version_info = Arc::new(parsed);
    self
      .memory_cache
      .lock()
      .versions
      .insert(package_nv.clone(), version_info.clone());
    Ok(Some(version_info))
  }

  pub async fn maybe_package_info(
    self: &Arc<Self>,
    name: &str,
  ) -> Result<Option<Arc<SmallNpmPackageInfo>>, LoadPackageInfoError> {
    self
      .load_package_info_inner(name)
      .await
      .map_err(|err| LoadPackageInfoError {
        url: get_package_url(&self.npmrc, name),
        name: name.to_string(),
        inner: err,
      })
      .map(|maybe_info| maybe_info.map(|info| info.versions.clone()))
  }

  async fn load_package_info_inner(
    self: &Arc<Self>,
    name: &str,
  ) -> Result<Option<Arc<CacheInfo>>, LoadPackageInfoInnerError> {
    let (cache_item, clear_id) = {
      let mut mem_cache = self.memory_cache.lock();
      let cache_item = if let Some(cache_item) = mem_cache.get(name) {
        cache_item.clone()
      } else {
        let value_creator = MultiRuntimeAsyncValueCreator::new({
          let downloader = self.clone();
          let name = name.to_string();
          Box::new(move || downloader.create_load_future(&name))
        });
        let cache_item = MemoryCacheItem::Pending(Arc::new(value_creator));
        mem_cache.insert(name.to_string(), cache_item.clone());
        cache_item
      };
      (cache_item, mem_cache.clear_id)
    };

    match cache_item {
      MemoryCacheItem::FsCached(info) => {
        // // this struct previously loaded from the registry, so we can load it from the file system cache
        // self
        //   .load_file_cached_package_info(name)
        //   .await
        //   .map(Some)
        //   .map_err(LoadPackageInfoInnerError::LoadFileCachedPackageInfo)
        Ok(Some(info.clone()))
      }
      MemoryCacheItem::MemoryCached(maybe_info) => {
        maybe_info.clone().map_err(LoadPackageInfoInnerError::Other)
      }
      MemoryCacheItem::Pending(value_creator) => {
        match value_creator.get().await {
          Ok(FutureResult::SavedFsCache(info)) => {
            // return back the future and mark this package as having
            // been saved in the cache for next time it's requested
            self.memory_cache.lock().try_insert(
              clear_id,
              name,
              MemoryCacheItem::FsCached(info.clone()),
            );
            Ok(Some(info.clone()))
          }
          Ok(FutureResult::ErroredFsCache(info)) => {
            // since saving to the fs cache failed, keep the package information in memory
            self.memory_cache.lock().try_insert(
              clear_id,
              name,
              MemoryCacheItem::MemoryCached(Ok(Some(info.clone()))),
            );
            Ok(Some(info.clone()))
          }
          Ok(FutureResult::PackageNotExists) => {
            self.memory_cache.lock().try_insert(
              clear_id,
              name,
              MemoryCacheItem::MemoryCached(Ok(None)),
            );
            Ok(None)
          }
          Err(err) => {
            let return_err = err.clone();
            self.memory_cache.lock().try_insert(
              clear_id,
              name,
              MemoryCacheItem::MemoryCached(Err(err)),
            );
            Err(LoadPackageInfoInnerError::Other(return_err))
          }
        }
      }
    }
  }

  async fn load_file_cached_package_info(
    &self,
    name: &str,
  ) -> Result<Arc<CacheInfo>, LoadFileCachedPackageInfoError> {
    // this scenario failing should be exceptionally rare so let's
    // deal with improving it only when anyone runs into an issue
    let maybe_package_info = deno_unsync::spawn_blocking({
      let cache = self.cache.clone();
      let name = name.to_string();
      move || cache.load_package_info(&name)
    })
    .await
    .unwrap()
    .map_err(|err| LoadFileCachedPackageInfoError::LoadPackageInfo {
      err,
      name: name.to_string(),
    })?;
    match maybe_package_info {
      Some(package_info) => Ok(Arc::new(package_info)),
      None => Err(LoadFileCachedPackageInfoError::FileMissing(
        name.to_string(),
      )),
    }
  }

  fn create_load_future(self: &Arc<Self>, name: &str) -> LoadFuture {
    let downloader = self.clone();
    let package_url = get_package_url(&self.npmrc, name);
    let registry_config = self.npmrc.get_registry_config(name);
    let maybe_auth_header =
      match maybe_auth_header_for_npm_registry(registry_config) {
        Ok(maybe_auth_header) => maybe_auth_header,
        Err(err) => {
          return std::future::ready(Err(Arc::new(JsErrorBox::from_err(err))))
            .boxed_local()
        }
      };
    let name = name.to_string();
    async move {
      if (downloader.cache.cache_setting().should_use_for_npm_package(&name) && !downloader.force_reload_flag.is_raised())
        // if this has been previously reloaded, then try loading from the
        // file system cache
        || downloader.previously_loaded_packages.lock().contains(&name)
      {
        // attempt to load from the file cache
        if let Some(info) = downloader.cache.load_package_info(&name).map_err(JsErrorBox::from_err)? {
          let result = Arc::new(info);
          return Ok(FutureResult::SavedFsCache(result));
        }
      }

      if *downloader.cache.cache_setting() == NpmCacheSetting::Only {
        return Err(JsErrorBox::new(
          "NotCached",
          format!(
            "npm package not found in cache: \"{name}\", --cached-only is specified."
          )
        ));
      }

      downloader.previously_loaded_packages.lock().insert(name.to_string());

      let maybe_bytes = downloader
        .http_client
        .download_with_retries_on_any_tokio_runtime(
          package_url,
          maybe_auth_header,
        )
        .await.map_err(JsErrorBox::from_err)?;
      match maybe_bytes {
        Some(bytes) => {
          let future_result = deno_unsync::spawn_blocking(
            move || -> Result<FutureResult, JsErrorBox> {
              let string = String::from_utf8(bytes).unwrap();
              let package_info = CacheInfo::new(name.to_string(), string);
              match downloader.cache.save_package_info(&name, &package_info) {
                Ok(()) => {
                  Ok(FutureResult::SavedFsCache(Arc::new(package_info)))
                }
                Err(err) => {
                  log::debug!(
                    "Error saving package {} to cache: {:#}",
                    name,
                    err
                  );
                  Ok(FutureResult::ErroredFsCache(Arc::new(package_info)))
                }
              }
            },
          )
          .await
          .map_err(JsErrorBox::from_err)??;
          Ok(future_result)
        }
        None => Ok(FutureResult::PackageNotExists),
      }
    }
    .map(|r| r.map_err(Arc::new))
    .boxed_local()
  }
}

pub struct NpmRegistryApiAdapter<
  THttpClient: NpmCacheHttpClient,
  TSys: FsCreateDirAll
    + FsHardLink
    + FsMetadata
    + FsOpen
    + FsReadDir
    + FsRemoveFile
    + FsRename
    + ThreadSleep
    + SystemRandom
    + Send
    + Sync
    + 'static,
>(Arc<RegistryInfoProvider<THttpClient, TSys>>);

#[async_trait(?Send)]
impl<
    THttpClient: NpmCacheHttpClient,
    TSys: FsCreateDirAll
      + FsHardLink
      + FsMetadata
      + FsOpen
      + FsReadDir
      + FsRemoveFile
      + FsRename
      + ThreadSleep
      + SystemRandom
      + Send
      + Sync
      + 'static,
  > NpmRegistryApi for NpmRegistryApiAdapter<THttpClient, TSys>
{
  // async fn package_info(
  //   &self,
  //   name: &str,
  // ) -> Result<Arc<NpmPackageInfo>, NpmRegistryPackageInfoLoadError> {
  //   self.0.package_info(name).await
  // }

  async fn package_versions(
    &self,
    name: &str,
  ) -> Result<Arc<SmallNpmPackageInfo>, NpmRegistryPackageInfoLoadError> {
    // self.0.package_versions(name).await
    self.0.package_info(name).await
  }

  async fn package_version_info(
    &self,
    package_nv: &PackageNv,
    _: &HashMap<StackString, Vec<NpmPackageVersionInfo>>,
  ) -> Result<Arc<NpmPackageVersionInfo>, NpmRegistryPackageInfoLoadError> {
    // self.0.package_version_info(package_nv).await
    self
      .0
      .maybe_package_version_info(package_nv)
      .await
      .map_err(|err| {
        NpmRegistryPackageInfoLoadError::LoadError(Arc::new(
          JsErrorBox::from_err(err),
        ))
      })?
      .ok_or_else(|| {
        NpmRegistryPackageInfoLoadError::VersionNotFound(
          deno_npm::resolution::NpmPackageVersionNotFound(package_nv.clone()),
        )
      })
  }

  fn mark_force_reload(&self) -> bool {
    self.0.mark_force_reload()
  }
}

// todo(#27198): make this private and only use RegistryInfoProvider in the rest of
// the code
pub fn get_package_url(npmrc: &ResolvedNpmRc, name: &str) -> Url {
  let registry_url = npmrc.get_registry_url(name);
  // The '/' character in scoped package names "@scope/name" must be
  // encoded for older third party registries. Newer registries and
  // npm itself support both ways
  //   - encoded: https://registry.npmjs.org/@rollup%2fplugin-json
  //   - non-ecoded: https://registry.npmjs.org/@rollup/plugin-json
  // To support as many third party registries as possible we'll
  // always encode the '/' character.

  // list of all characters used in npm packages:
  //  !, ', (, ), *, -, ., /, [0-9], @, [A-Za-z], _, ~
  const ASCII_SET: percent_encoding::AsciiSet =
    percent_encoding::NON_ALPHANUMERIC
      .remove(b'!')
      .remove(b'\'')
      .remove(b'(')
      .remove(b')')
      .remove(b'*')
      .remove(b'-')
      .remove(b'.')
      .remove(b'@')
      .remove(b'_')
      .remove(b'~');
  let name = percent_encoding::utf8_percent_encode(name, &ASCII_SET);
  registry_url
    // Ensure that scoped package name percent encoding is lower cased
    // to match npm.
    .join(&name.to_string().replace("%2F", "%2f"))
    .unwrap()
}
