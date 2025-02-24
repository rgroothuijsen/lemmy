use crate::Perform;
use actix_web::web::Data;
use lemmy_api_common::{
  context::LemmyContext,
  person::{GetRepliesResponse, MarkAllAsRead},
  utils::get_local_user_view_from_jwt,
};
use lemmy_db_schema::source::{
  comment_reply::CommentReply,
  person_mention::PersonMention,
  private_message::PrivateMessage,
};
use lemmy_utils::{error::LemmyError, ConnectionId};

#[async_trait::async_trait(?Send)]
impl Perform for MarkAllAsRead {
  type Response = GetRepliesResponse;

  #[tracing::instrument(skip(context, _websocket_id))]
  async fn perform(
    &self,
    context: &Data<LemmyContext>,
    _websocket_id: Option<ConnectionId>,
  ) -> Result<GetRepliesResponse, LemmyError> {
    let data: &MarkAllAsRead = self;
    let local_user_view =
      get_local_user_view_from_jwt(&data.auth, context.pool(), context.secret()).await?;
    let person_id = local_user_view.person.id;

    // Mark all comment_replies as read
    CommentReply::mark_all_as_read(context.pool(), person_id)
      .await
      .map_err(|e| LemmyError::from_error_message(e, "couldnt_update_comment"))?;

    // Mark all user mentions as read
    PersonMention::mark_all_as_read(context.pool(), person_id)
      .await
      .map_err(|e| LemmyError::from_error_message(e, "couldnt_update_comment"))?;

    // Mark all private_messages as read
    PrivateMessage::mark_all_as_read(context.pool(), person_id)
      .await
      .map_err(|e| LemmyError::from_error_message(e, "couldnt_update_private_message"))?;

    Ok(GetRepliesResponse { replies: vec![] })
  }
}
