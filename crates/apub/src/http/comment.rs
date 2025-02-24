use crate::{
  http::{create_apub_response, create_apub_tombstone_response},
  objects::comment::ApubComment,
};
use activitypub_federation::traits::ApubObject;
use actix_web::{web, web::Path, HttpResponse};
use diesel::result::Error::NotFound;
use lemmy_api_common::context::LemmyContext;
use lemmy_db_schema::{newtypes::CommentId, source::comment::Comment, traits::Crud};
use lemmy_utils::error::LemmyError;
use serde::Deserialize;

#[derive(Deserialize)]
pub(crate) struct CommentQuery {
  comment_id: String,
}

/// Return the ActivityPub json representation of a local comment over HTTP.
#[tracing::instrument(skip_all)]
pub(crate) async fn get_apub_comment(
  info: Path<CommentQuery>,
  context: web::Data<LemmyContext>,
) -> Result<HttpResponse, LemmyError> {
  let id = CommentId(info.comment_id.parse::<i32>()?);
  let comment: ApubComment = Comment::read(context.pool(), id).await?.into();
  if !comment.local {
    return Err(NotFound.into());
  }

  if !comment.deleted && !comment.removed {
    Ok(create_apub_response(&comment.into_apub(&context).await?))
  } else {
    Ok(create_apub_tombstone_response(comment.ap_id.clone()))
  }
}
