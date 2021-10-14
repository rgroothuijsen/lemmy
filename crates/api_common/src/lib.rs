pub mod comment;
pub mod community;
pub mod person;
pub mod post;
pub mod site;
pub mod websocket;

use crate::site::FederatedInstances;
use diesel::PgConnection;
use lemmy_db_queries::{
  source::{community::Community_, person_block::PersonBlock_, site::Site_},
  Crud,
  DbPool,
  Readable,
};
use lemmy_db_schema::{
  source::{
    comment::Comment,
    community::Community,
    person::Person,
    person_block::PersonBlock,
    person_mention::{PersonMention, PersonMentionForm},
    post::{Post, PostRead, PostReadForm},
    secret::Secret,
    site::Site,
  },
  CommunityId,
  LocalUserId,
  PersonId,
  PostId,
};
use lemmy_db_views::local_user_view::{LocalUserSettingsView, LocalUserView};
use lemmy_db_views_actor::{
  community_person_ban_view::CommunityPersonBanView,
  community_view::CommunityView,
};
use lemmy_utils::{
  claims::Claims,
  email::send_email,
  settings::structs::{FederationConfig, Settings},
  utils::MentionData,
  ApiError,
  LemmyError,
};
use log::error;
use url::Url;

pub async fn blocking<F, T>(pool: &DbPool, f: F) -> Result<T, LemmyError>
where
  F: FnOnce(&diesel::PgConnection) -> T + Send + 'static,
  T: Send + 'static,
{
  let pool = pool.clone();
  let res = actix_web::web::block(move || {
    let conn = pool.get()?;
    let res = (f)(&conn);
    Ok(res) as Result<T, LemmyError>
  })
  .await?;

  res
}

pub async fn send_local_notifs(
  mentions: Vec<MentionData>,
  comment: Comment,
  person: Person,
  post: Post,
  pool: &DbPool,
  do_send_email: bool,
  settings: &Settings,
) -> Result<Vec<LocalUserId>, LemmyError> {
  let settings = settings.to_owned();
  let ids = blocking(pool, move |conn| {
    do_send_local_notifs(
      conn,
      &mentions,
      &comment,
      &person,
      &post,
      do_send_email,
      &settings,
    )
  })
  .await?;

  Ok(ids)
}

fn do_send_local_notifs(
  conn: &PgConnection,
  mentions: &[MentionData],
  comment: &Comment,
  person: &Person,
  post: &Post,
  do_send_email: bool,
  settings: &Settings,
) -> Vec<LocalUserId> {
  let mut recipient_ids = Vec::new();

  // Send the local mentions
  for mention in mentions
    .iter()
    .filter(|m| m.is_local(&settings.hostname) && m.name.ne(&person.name))
    .collect::<Vec<&MentionData>>()
  {
    if let Ok(mention_user_view) = LocalUserView::read_from_name(conn, &mention.name) {
      // TODO
      // At some point, make it so you can't tag the parent creator either
      // This can cause two notifications, one for reply and the other for mention
      recipient_ids.push(mention_user_view.local_user.id);

      let user_mention_form = PersonMentionForm {
        recipient_id: mention_user_view.person.id,
        comment_id: comment.id,
        read: None,
      };

      // Allow this to fail softly, since comment edits might re-update or replace it
      // Let the uniqueness handle this fail
      PersonMention::create(conn, &user_mention_form).ok();

      // Send an email to those local users that have notifications on
      if do_send_email {
        send_email_to_user(
          &mention_user_view,
          "Mentioned by",
          "Person Mention",
          &comment.content,
          settings,
        )
      }
    }
  }

  // Send notifs to the parent commenter / poster
  match comment.parent_id {
    Some(parent_id) => {
      if let Ok(parent_comment) = Comment::read(conn, parent_id) {
        // Don't send a notif to yourself
        if parent_comment.creator_id != person.id {
          // Get the parent commenter local_user
          if let Ok(parent_user_view) = LocalUserView::read_person(conn, parent_comment.creator_id)
          {
            recipient_ids.push(parent_user_view.local_user.id);

            if do_send_email {
              send_email_to_user(
                &parent_user_view,
                "Reply from",
                "Comment Reply",
                &comment.content,
                settings,
              )
            }
          }
        }
      }
    }
    // Its a post
    None => {
      if post.creator_id != person.id {
        if let Ok(parent_user_view) = LocalUserView::read_person(conn, post.creator_id) {
          recipient_ids.push(parent_user_view.local_user.id);

          if do_send_email {
            send_email_to_user(
              &parent_user_view,
              "Reply from",
              "Post Reply",
              &comment.content,
              settings,
            )
          }
        }
      }
    }
  };
  recipient_ids
}

pub fn send_email_to_user(
  local_user_view: &LocalUserView,
  subject_text: &str,
  body_text: &str,
  comment_content: &str,
  settings: &Settings,
) {
  if local_user_view.person.banned || !local_user_view.local_user.send_notifications_to_email {
    return;
  }

  if let Some(user_email) = &local_user_view.local_user.email {
    let subject = &format!(
      "{} - {} {}",
      subject_text, settings.hostname, local_user_view.person.name,
    );
    let html = &format!(
      "<h1>{}</h1><br><div>{} - {}</div><br><a href={}/inbox>inbox</a>",
      body_text,
      local_user_view.person.name,
      comment_content,
      settings.get_protocol_and_hostname()
    );
    match send_email(
      subject,
      user_email,
      &local_user_view.person.name,
      html,
      settings,
    ) {
      Ok(_o) => _o,
      Err(e) => error!("{}", e),
    };
  }
}

pub async fn is_mod_or_admin(
  pool: &DbPool,
  person_id: PersonId,
  community_id: CommunityId,
) -> Result<(), LemmyError> {
  let is_mod_or_admin = blocking(pool, move |conn| {
    CommunityView::is_mod_or_admin(conn, person_id, community_id)
  })
  .await?;
  if !is_mod_or_admin {
    return Err(ApiError::err_plain("not_a_mod_or_admin").into());
  }
  Ok(())
}

pub fn is_admin(local_user_view: &LocalUserView) -> Result<(), LemmyError> {
  if !local_user_view.person.admin {
    return Err(ApiError::err_plain("not_an_admin").into());
  }
  Ok(())
}

pub async fn get_post(post_id: PostId, pool: &DbPool) -> Result<Post, LemmyError> {
  blocking(pool, move |conn| Post::read(conn, post_id))
    .await?
    .map_err(|_| ApiError::err_plain("couldnt_find_post").into())
}

pub async fn mark_post_as_read(
  person_id: PersonId,
  post_id: PostId,
  pool: &DbPool,
) -> Result<PostRead, LemmyError> {
  let post_read_form = PostReadForm { post_id, person_id };

  blocking(pool, move |conn| {
    PostRead::mark_as_read(conn, &post_read_form)
  })
  .await?
  .map_err(|_| ApiError::err_plain("couldnt_mark_post_as_read").into())
}

pub async fn get_local_user_view_from_jwt(
  jwt: &str,
  pool: &DbPool,
  secret: &Secret,
) -> Result<LocalUserView, LemmyError> {
  let claims = Claims::decode(jwt, &secret.jwt_secret)
    .map_err(|e| ApiError::err("not_logged_in", e))?
    .claims;
  let local_user_id = LocalUserId(claims.sub);
  let local_user_view =
    blocking(pool, move |conn| LocalUserView::read(conn, local_user_id)).await??;
  // Check for a site ban
  if local_user_view.person.banned {
    return Err(ApiError::err_plain("site_ban").into());
  }

  // Check for user deletion
  if local_user_view.person.deleted {
    return Err(ApiError::err_plain("deleted").into());
  }

  check_validator_time(&local_user_view.local_user.validator_time, &claims)?;

  Ok(local_user_view)
}

/// Checks if user's token was issued before user's password reset.
pub fn check_validator_time(
  validator_time: &chrono::NaiveDateTime,
  claims: &Claims,
) -> Result<(), LemmyError> {
  let user_validation_time = validator_time.timestamp();
  if user_validation_time > claims.iat {
    Err(ApiError::err_plain("not_logged_in").into())
  } else {
    Ok(())
  }
}

pub async fn get_local_user_view_from_jwt_opt(
  jwt: &Option<String>,
  pool: &DbPool,
  secret: &Secret,
) -> Result<Option<LocalUserView>, LemmyError> {
  match jwt {
    Some(jwt) => Ok(Some(get_local_user_view_from_jwt(jwt, pool, secret).await?)),
    None => Ok(None),
  }
}

pub async fn get_local_user_settings_view_from_jwt(
  jwt: &str,
  pool: &DbPool,
  secret: &Secret,
) -> Result<LocalUserSettingsView, LemmyError> {
  let claims = Claims::decode(jwt, &secret.jwt_secret)
    .map_err(|e| ApiError::err("not_logged_in", e))?
    .claims;
  let local_user_id = LocalUserId(claims.sub);
  let local_user_view = blocking(pool, move |conn| {
    LocalUserSettingsView::read(conn, local_user_id)
  })
  .await??;
  // Check for a site ban
  if local_user_view.person.banned {
    return Err(ApiError::err_plain("site_ban").into());
  }

  check_validator_time(&local_user_view.local_user.validator_time, &claims)?;

  Ok(local_user_view)
}

pub async fn get_local_user_settings_view_from_jwt_opt(
  jwt: &Option<String>,
  pool: &DbPool,
  secret: &Secret,
) -> Result<Option<LocalUserSettingsView>, LemmyError> {
  match jwt {
    Some(jwt) => Ok(Some(
      get_local_user_settings_view_from_jwt(jwt, pool, secret).await?,
    )),
    None => Ok(None),
  }
}

pub async fn check_community_ban(
  person_id: PersonId,
  community_id: CommunityId,
  pool: &DbPool,
) -> Result<(), LemmyError> {
  let is_banned =
    move |conn: &'_ _| CommunityPersonBanView::get(conn, person_id, community_id).is_ok();
  if blocking(pool, is_banned).await? {
    Err(ApiError::err_plain("community_ban").into())
  } else {
    Ok(())
  }
}

pub async fn check_community_deleted_or_removed(
  community_id: CommunityId,
  pool: &DbPool,
) -> Result<(), LemmyError> {
  let community = blocking(pool, move |conn| Community::read(conn, community_id))
    .await?
    .map_err(|e| ApiError::err("couldnt_find_community", e))?;
  if community.deleted || community.removed {
    Err(ApiError::err_plain("deleted").into())
  } else {
    Ok(())
  }
}

pub fn check_post_deleted_or_removed(post: &Post) -> Result<(), LemmyError> {
  if post.deleted || post.removed {
    Err(ApiError::err_plain("deleted").into())
  } else {
    Ok(())
  }
}

pub async fn check_person_block(
  my_id: PersonId,
  potential_blocker_id: PersonId,
  pool: &DbPool,
) -> Result<(), LemmyError> {
  let is_blocked = move |conn: &'_ _| PersonBlock::read(conn, potential_blocker_id, my_id).is_ok();
  if blocking(pool, is_blocked).await? {
    Err(ApiError::err_plain("person_block").into())
  } else {
    Ok(())
  }
}

pub async fn check_downvotes_enabled(score: i16, pool: &DbPool) -> Result<(), LemmyError> {
  if score == -1 {
    let site = blocking(pool, move |conn| Site::read_simple(conn)).await??;
    if !site.enable_downvotes {
      return Err(ApiError::err_plain("downvotes_disabled").into());
    }
  }
  Ok(())
}

pub async fn build_federated_instances(
  pool: &DbPool,
  federation_config: &FederationConfig,
  hostname: &str,
) -> Result<Option<FederatedInstances>, LemmyError> {
  let federation = federation_config.to_owned();
  if federation.enabled {
    let distinct_communities = blocking(pool, move |conn| {
      Community::distinct_federated_communities(conn)
    })
    .await??;

    let allowed = federation.allowed_instances;
    let blocked = federation.blocked_instances;

    let mut linked = distinct_communities
      .iter()
      .map(|actor_id| Ok(Url::parse(actor_id)?.host_str().unwrap_or("").to_string()))
      .collect::<Result<Vec<String>, LemmyError>>()?;

    if let Some(allowed) = allowed.as_ref() {
      linked.extend_from_slice(allowed);
    }

    if let Some(blocked) = blocked.as_ref() {
      linked.retain(|a| !blocked.contains(a) && !a.eq(hostname));
    }

    // Sort and remove dupes
    linked.sort_unstable();
    linked.dedup();

    Ok(Some(FederatedInstances {
      linked,
      allowed,
      blocked,
    }))
  } else {
    Ok(None)
  }
}

/// Checks the password length
pub fn password_length_check(pass: &str) -> Result<(), LemmyError> {
  if !(10..=60).contains(&pass.len()) {
    Err(ApiError::err_plain("invalid_password").into())
  } else {
    Ok(())
  }
}

/// Checks the site description length
pub fn site_description_length_check(description: &str) -> Result<(), LemmyError> {
  if description.len() > 150 {
    Err(ApiError::err_plain("site_description_length_overflow").into())
  } else {
    Ok(())
  }
}

/// Checks for a honeypot. If this field is filled, fail the rest of the function
pub fn honeypot_check(honeypot: &Option<String>) -> Result<(), LemmyError> {
  if honeypot.is_some() {
    Err(ApiError::err_plain("honeypot_fail").into())
  } else {
    Ok(())
  }
}
