use super::{handle_community_moderators, person::ApubPerson};
use crate::{
  activities::GetActorType,
  fetcher::markdown_links::markdown_rewrite_remote_links_opt,
  objects::{instance::fetch_instance_actor_for_object, read_from_string_or_source_opt},
  protocol::{
    objects::{group::Group, AttributedTo, LanguageTag, PersonOrGroupType},
    ImageObject,
    Source,
  },
};
use activitypub_federation::{
  config::Data,
  fetch::object_id::ObjectId,
  kinds::actor::GroupType,
  protocol::values::MediaTypeHtml,
  traits::{Actor, Object},
};
use chrono::{DateTime, Utc};
use lemmy_api_common::{
  context::LemmyContext,
  utils::{
    check_nsfw_allowed,
    generate_featured_url,
    generate_moderators_url,
    generate_outbox_url,
    get_url_blocklist,
    process_markdown_opt,
    proxy_image_link_opt_apub,
    slur_regex,
  },
};
use lemmy_db_schema::{
  sensitive::SensitiveString,
  source::{
    actor_language::CommunityLanguage,
    community::{Community, CommunityInsertForm, CommunityUpdateForm},
  },
  traits::{ApubActor, Crud},
};
use lemmy_db_schema_file::enums::{ActorType, CommunityVisibility};
use lemmy_db_views_site::SiteView;
use lemmy_utils::{
  error::{LemmyError, LemmyResult},
  spawn_try_task,
  utils::{markdown::markdown_to_html, validation::truncate_description},
};
use std::ops::Deref;
use url::Url;

#[derive(Clone, Debug)]
pub struct ApubCommunity(Community);

impl Deref for ApubCommunity {
  type Target = Community;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<Community> for ApubCommunity {
  fn from(c: Community) -> Self {
    ApubCommunity(c)
  }
}

#[async_trait::async_trait]
impl Object for ApubCommunity {
  type DataType = LemmyContext;
  type Kind = Group;
  type Error = LemmyError;

  fn last_refreshed_at(&self) -> Option<DateTime<Utc>> {
    Some(self.last_refreshed_at)
  }

  async fn read_from_id(
    object_id: Url,
    context: &Data<Self::DataType>,
  ) -> LemmyResult<Option<Self>> {
    Ok(
      Community::read_from_apub_id(&mut context.pool(), &object_id.into())
        .await?
        .map(Into::into),
    )
  }

  async fn delete(self, context: &Data<Self::DataType>) -> LemmyResult<()> {
    let form = CommunityUpdateForm {
      deleted: Some(true),
      ..Default::default()
    };
    Community::update(&mut context.pool(), self.id, &form).await?;
    Ok(())
  }

  async fn into_json(self, data: &Data<Self::DataType>) -> LemmyResult<Group> {
    let community_id = self.id;
    let langs = CommunityLanguage::read(&mut data.pool(), community_id).await?;
    let language = LanguageTag::new_multiple(langs, &mut data.pool()).await?;

    let group = Group {
      kind: GroupType::Group,
      id: self.id().into(),
      preferred_username: self.name.clone(),
      name: Some(self.title.clone()),
      content: self.sidebar.as_ref().map(|d| markdown_to_html(d)),
      source: self.sidebar.clone().map(Source::new),
      summary: self.description.clone(),
      media_type: self.sidebar.as_ref().map(|_| MediaTypeHtml::Html),
      icon: self.icon.clone().map(ImageObject::new),
      image: self.banner.clone().map(ImageObject::new),
      sensitive: Some(self.nsfw),
      featured: Some(generate_featured_url(&self.ap_id)?.into()),
      inbox: self.inbox_url.clone().into(),
      outbox: generate_outbox_url(&self.ap_id)?.into(),
      followers: self.followers_url.clone().map(Into::into),
      endpoints: None,
      public_key: self.public_key(),
      language,
      published: Some(self.published),
      updated: self.updated,
      posting_restricted_to_mods: Some(self.posting_restricted_to_mods),
      attributed_to: Some(AttributedTo::Lemmy(
        generate_moderators_url(&self.ap_id)?.into(),
      )),
      manually_approves_followers: Some(self.visibility == CommunityVisibility::Private),
      discoverable: Some(self.visibility != CommunityVisibility::Unlisted),
    };
    Ok(group)
  }

  async fn verify(
    group: &Group,
    expected_domain: &Url,
    context: &Data<Self::DataType>,
  ) -> LemmyResult<()> {
    group.verify(expected_domain, context).await
  }

  /// Converts a `Group` to `Community`, inserts it into the database and updates moderators.
  async fn from_json(group: Group, context: &Data<Self::DataType>) -> LemmyResult<ApubCommunity> {
    let local_site = SiteView::read_local(&mut context.pool())
      .await
      .ok()
      .map(|s| s.local_site);
    let instance_id = fetch_instance_actor_for_object(&group.id, context).await?;

    let slur_regex = slur_regex(context).await?;
    let url_blocklist = get_url_blocklist(context).await?;
    let sidebar = read_from_string_or_source_opt(&group.content, &None, &group.source);
    let sidebar = process_markdown_opt(&sidebar, &slur_regex, &url_blocklist, context).await?;
    let sidebar = markdown_rewrite_remote_links_opt(sidebar, context).await;
    let icon = proxy_image_link_opt_apub(group.icon.map(|i| i.url), context).await?;
    let banner = proxy_image_link_opt_apub(group.image.map(|i| i.url), context).await?;
    let visibility = Some(if group.manually_approves_followers.unwrap_or_default() {
      CommunityVisibility::Private
    } else if !group.discoverable.unwrap_or(true) {
      CommunityVisibility::Unlisted
    } else {
      CommunityVisibility::Public
    });

    // If NSFW is not allowed, then remove NSFW communities
    let removed = check_nsfw_allowed(group.sensitive, local_site.as_ref())
      .err()
      .map(|_| true);

    let form = CommunityInsertForm {
      published: group.published,
      updated: group.updated,
      deleted: Some(false),
      nsfw: Some(group.sensitive.unwrap_or(false)),
      ap_id: Some(group.id.into()),
      local: Some(false),
      last_refreshed_at: Some(Utc::now()),
      icon,
      banner,
      sidebar,
      removed,
      description: group.summary.as_deref().map(truncate_description),
      followers_url: group.followers.clone().map(Into::into),
      inbox_url: Some(
        group
          .endpoints
          .map(|e| e.shared_inbox)
          .unwrap_or(group.inbox)
          .into(),
      ),
      moderators_url: group.attributed_to.clone().and_then(AttributedTo::url),
      posting_restricted_to_mods: group.posting_restricted_to_mods,
      featured_url: group.featured.clone().map(Into::into),
      visibility,
      ..CommunityInsertForm::new(
        instance_id,
        group.preferred_username.clone(),
        group.name.unwrap_or(group.preferred_username.clone()),
        group.public_key.public_key_pem,
      )
    };
    let languages =
      LanguageTag::to_language_id_multiple(group.language, &mut context.pool()).await?;

    let timestamp = group.updated.or(group.published).unwrap_or_else(Utc::now);
    let community = Community::insert_apub(&mut context.pool(), timestamp, &form).await?;
    CommunityLanguage::update(&mut context.pool(), languages, community.id).await?;

    let community: ApubCommunity = community.into();

    // These collections are not necessary for Lemmy to work, so ignore errors.
    let community_ = community.clone();
    let context_ = context.reset_request_count();
    spawn_try_task(async move {
      group.outbox.dereference(&community_, &context_).await.ok();
      if let Some(followers) = group.followers {
        followers.dereference(&community_, &context_).await.ok();
      }
      if let Some(featured) = group.featured {
        featured.dereference(&community_, &context_).await.ok();
      }
      if let Some(moderators) = group.attributed_to {
        if let AttributedTo::Lemmy(l) = moderators {
          l.moderators()
            .dereference(&community_, &context_)
            .await
            .ok();
        } else if let AttributedTo::Peertube(p) = moderators {
          let new_mods = p
            .iter()
            .filter(|p| p.kind == PersonOrGroupType::Person)
            .map(|p| ObjectId::<ApubPerson>::from(p.id.clone().into_inner()))
            .collect();
          handle_community_moderators(&new_mods, &community_, &context_)
            .await
            .ok();
        }
      }
      Ok(())
    });

    Ok(community)
  }
}

impl Actor for ApubCommunity {
  fn id(&self) -> Url {
    self.ap_id.inner().clone()
  }

  fn public_key_pem(&self) -> &str {
    &self.public_key
  }

  fn private_key_pem(&self) -> Option<String> {
    self.private_key.clone().map(SensitiveString::into_inner)
  }

  fn inbox(&self) -> Url {
    self.inbox_url.clone().into()
  }

  fn shared_inbox(&self) -> Option<Url> {
    None
  }
}

impl GetActorType for ApubCommunity {
  fn actor_type(&self) -> ActorType {
    ActorType::Community
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::{
    objects::instance::tests::parse_lemmy_instance,
    protocol::tests::file_to_json_object,
  };
  use activitypub_federation::fetch::collection_id::CollectionId;
  use lemmy_db_schema::source::site::Site;
  use pretty_assertions::assert_eq;
  use serial_test::serial;

  pub(crate) async fn parse_lemmy_community(
    context: &Data<LemmyContext>,
  ) -> LemmyResult<ApubCommunity> {
    // use separate counter so this doesn't affect tests
    let context2 = context.reset_request_count();
    let mut json: Group = file_to_json_object("assets/lemmy/objects/group.json")?;
    // change these links so they dont fetch over the network
    json.attributed_to = None;
    json.outbox = CollectionId::parse("https://enterprise.lemmy.ml/c/tenforward/not_outbox")?;
    json.followers = Some(CollectionId::parse(
      "https://enterprise.lemmy.ml/c/tenforward/not_followers",
    )?);

    let url = Url::parse("https://enterprise.lemmy.ml/c/tenforward")?;
    ApubCommunity::verify(&json, &url, &context2).await?;
    let community = ApubCommunity::from_json(json, &context2).await?;
    Ok(community)
  }

  #[tokio::test]
  #[serial]
  async fn test_parse_lemmy_community() -> LemmyResult<()> {
    let context = LemmyContext::init_test_context().await;
    let site = parse_lemmy_instance(&context).await?;
    let community = parse_lemmy_community(&context).await?;

    assert_eq!(community.title, "Ten Forward");
    assert!(!community.local);

    // Test the sidebar and description
    assert_eq!(
      community.sidebar.as_ref().map(std::string::String::len),
      Some(63)
    );
    assert_eq!(
      community.description,
      Some("A description of ten forward.".into())
    );

    Community::delete(&mut context.pool(), community.id).await?;
    Site::delete(&mut context.pool(), site.id).await?;
    Ok(())
  }
}
