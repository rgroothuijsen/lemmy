use crate::fetcher::PostOrComment;
use activitypub_federation::{
  config::{Data, UrlVerifier},
  error::Error as ActivityPubError,
};
use async_trait::async_trait;
use lemmy_api_common::context::LemmyContext;
use lemmy_db_schema::{
  source::{activity::ReceivedActivity, instance::Instance, local_site::LocalSite},
  utils::{ActualDbPool, DbPool},
};
use lemmy_db_views_site::SiteView;
use lemmy_utils::{
  error::{FederationError, LemmyError, LemmyErrorType, LemmyResult},
  CacheLock,
  CACHE_DURATION_FEDERATION,
};
use moka::future::Cache;
use serde_json::Value;
use std::sync::{Arc, LazyLock};
use tracing::debug;
use url::Url;

pub mod activities;
pub mod activity_lists;
pub mod api;
pub(crate) mod collections;
pub mod fetcher;
pub mod http;
pub(crate) mod mentions;
pub mod objects;
pub mod protocol;

/// Maximum number of outgoing HTTP requests to fetch a single object. Needs to be high enough
/// to fetch a new community with posts, moderators and featured posts.
pub const FEDERATION_HTTP_FETCH_LIMIT: u32 = 100;

/// Only include a basic context to save space and bandwidth. The main context is hosted statically
/// on join-lemmy.org. Include activitystreams explicitly for better compat, but this could
/// theoretically also be moved.
pub static FEDERATION_CONTEXT: LazyLock<Value> = LazyLock::new(|| {
  Value::Array(vec![
    Value::String("https://join-lemmy.org/context.json".to_string()),
    Value::String("https://www.w3.org/ns/activitystreams".to_string()),
  ])
});

#[derive(Clone)]
pub struct VerifyUrlData(pub ActualDbPool);

#[async_trait]
impl UrlVerifier for VerifyUrlData {
  async fn verify(&self, url: &Url) -> Result<(), ActivityPubError> {
    let local_site_data = local_site_data_cached(&mut (&self.0).into())
      .await
      .map_err(|e| ActivityPubError::Other(format!("Cant read local site data: {e}")))?;

    use FederationError::*;
    check_apub_id_valid(url, &local_site_data).map_err(|err| match err {
      LemmyError {
        error_type:
          LemmyErrorType::FederationError {
            error: Some(FederationDisabled),
          },
        ..
      } => ActivityPubError::Other("Federation disabled".into()),
      LemmyError {
        error_type:
          LemmyErrorType::FederationError {
            error: Some(DomainBlocked(domain)),
          },
        ..
      } => ActivityPubError::Other(format!("Domain {domain:?} is blocked")),
      LemmyError {
        error_type:
          LemmyErrorType::FederationError {
            error: Some(DomainNotInAllowList(domain)),
          },
        ..
      } => ActivityPubError::Other(format!("Domain {domain:?} is not in allowlist")),
      _ => ActivityPubError::Other("Failed validating apub id".into()),
    })?;
    Ok(())
  }
}

/// Checks if the ID is allowed for sending or receiving.
///
/// In particular, it checks for:
/// - federation being enabled (if its disabled, only local URLs are allowed)
/// - the correct scheme (either http or https)
/// - URL being in the allowlist (if it is active)
/// - URL not being in the blocklist (if it is active)
fn check_apub_id_valid(apub_id: &Url, local_site_data: &LocalSiteData) -> LemmyResult<()> {
  let domain = apub_id
    .domain()
    .ok_or(FederationError::UrlWithoutDomain)?
    .to_string();

  if !local_site_data
    .local_site
    .as_ref()
    .map(|l| l.federation_enabled)
    .unwrap_or(true)
  {
    Err(FederationError::FederationDisabled)?
  }

  if local_site_data
    .blocked_instances
    .iter()
    .any(|i| domain.to_lowercase().eq(&i.domain.to_lowercase()))
  {
    Err(FederationError::DomainBlocked(domain.clone()))?
  }

  // Only check this if there are instances in the allowlist
  if !local_site_data.allowed_instances.is_empty()
    && !local_site_data
      .allowed_instances
      .iter()
      .any(|i| domain.to_lowercase().eq(&i.domain.to_lowercase()))
  {
    Err(FederationError::DomainNotInAllowList(domain))?
  }

  Ok(())
}

#[derive(Clone)]
pub(crate) struct LocalSiteData {
  local_site: Option<LocalSite>,
  allowed_instances: Vec<Instance>,
  blocked_instances: Vec<Instance>,
}

pub(crate) async fn local_site_data_cached(
  pool: &mut DbPool<'_>,
) -> LemmyResult<Arc<LocalSiteData>> {
  // All incoming and outgoing federation actions read the blocklist/allowlist and slur filters
  // multiple times. This causes a huge number of database reads if we hit the db directly. So we
  // cache these values for a short time, which will already make a huge difference and ensures that
  // changes take effect quickly.
  static CACHE: CacheLock<Arc<LocalSiteData>> = LazyLock::new(|| {
    Cache::builder()
      .max_capacity(1)
      .time_to_live(CACHE_DURATION_FEDERATION)
      .build()
  });
  Ok(
    CACHE
      .try_get_with((), async {
        let (local_site, allowed_instances, blocked_instances) =
          lemmy_db_schema::try_join_with_pool!(pool => (
            // LocalSite may be missing
            |pool| async {
              Ok(SiteView::read_local(pool).await.ok().map(|s| s.local_site))
            },
            Instance::allowlist,
            Instance::blocklist
          ))?;

        Ok::<_, LemmyError>(Arc::new(LocalSiteData {
          local_site,
          allowed_instances,
          blocked_instances,
        }))
      })
      .await.map_err(|e| anyhow::anyhow!("err getting activity: {e:?}"))?
  )
}

pub(crate) async fn check_apub_id_valid_with_strictness(
  apub_id: &Url,
  is_strict: bool,
  context: &LemmyContext,
) -> LemmyResult<()> {
  let domain = apub_id
    .domain()
    .ok_or(FederationError::UrlWithoutDomain)?
    .to_string();
  let local_instance = context.settings().get_hostname_without_port()?;
  if domain == local_instance {
    return Ok(());
  }

  let local_site_data = local_site_data_cached(&mut context.pool()).await?;
  check_apub_id_valid(apub_id, &local_site_data)?;

  // Only check allowlist if this is a community, and there are instances in the allowlist
  if is_strict && !local_site_data.allowed_instances.is_empty() {
    // need to allow this explicitly because apub receive might contain objects from our local
    // instance.
    let mut allowed_and_local = local_site_data
      .allowed_instances
      .iter()
      .map(|i| i.domain.clone())
      .collect::<Vec<String>>();
    let local_instance = context.settings().get_hostname_without_port()?;
    allowed_and_local.push(local_instance);

    let domain = apub_id
      .domain()
      .ok_or(FederationError::UrlWithoutDomain)?
      .to_string();
    if !allowed_and_local.contains(&domain) {
      Err(FederationError::FederationDisabledByStrictAllowList)?
    }
  }
  Ok(())
}

/// Store received activities in the database.
///
/// This ensures that the same activity doesn't get received and processed more than once, which
/// would be a waste of resources.
async fn insert_received_activity(ap_id: &Url, data: &Data<LemmyContext>) -> LemmyResult<()> {
  debug!("Received activity {}", ap_id.to_string());
  ReceivedActivity::create(&mut data.pool(), &ap_id.clone().into()).await?;
  Ok(())
}
