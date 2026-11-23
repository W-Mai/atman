//! Read one image from a committed temporary conversation position.

use std::io;

use anyhow::Result;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    attachment_store::{CONVERSATION_IMAGE_BYTES, read_retained_image},
    flow_store::{
        branches::ConversationItemId,
        position::{FlowPosition, TransientPosition},
    },
};

use super::{QuestionTarget, QuestionUnavailable, RuntimeHost, transcript::SOURCE_BYTES};

static READERS: Semaphore = Semaphore::const_new(2);

pub struct QuestionImageRequest {
    pub target: QuestionTarget,
    pub user: Uuid,
    pub position: TransientPosition,
    pub item: ConversationItemId,
    pub part: u32,
}

pub enum QuestionImageState {
    Ready {
        media_type: String,
        bytes: Vec<u8>,
    },
    Capacity {
        required_at_least: usize,
        allowed: usize,
    },
    ByteLimit {
        allowed: usize,
    },
    ReadersBusy,
    Unavailable,
    Retired,
}

impl RuntimeHost {
    pub async fn read_question_image(
        &self,
        request: QuestionImageRequest,
    ) -> Result<QuestionImageState> {
        use QuestionImageState as State;

        let QuestionImageRequest {
            target,
            user,
            position,
            item,
            part,
        } = request;
        let worker = match self.question_presentation(&target, user, false).await? {
            Ok((worker, _)) => worker,
            Err(QuestionUnavailable::Unavailable) => return Ok(State::Unavailable),
            Err(QuestionUnavailable::Retired) => return Ok(State::Retired),
        };
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(State::ReadersBusy);
        };
        let position = FlowPosition::Transient {
            transient: position,
        };
        let captured = worker
            .request(move |store| store.capture_transient_image_batch(&position, SOURCE_BYTES))
            .await;
        let captured = match captured {
            Ok(Some(Ok(captured))) => captured,
            Ok(Some(Err(required_at_least))) => {
                return Ok(State::Capacity {
                    required_at_least,
                    allowed: SOURCE_BYTES,
                });
            }
            _ if worker.is_stopping() || worker.is_stopped() => return Ok(State::Retired),
            _ => return Ok(State::Unavailable),
        };
        let result = crate::read_work::capture(move || -> io::Result<State> {
            let _permit = permit;
            let Some(image) = captured.image(item, part)? else {
                return Ok(State::Unavailable);
            };
            Ok(
                match read_retained_image(&image, CONVERSATION_IMAGE_BYTES, &Default::default()) {
                    Ok(bytes) => State::Ready {
                        media_type: image.media_type,
                        bytes,
                    },
                    Err(error) if error.kind() == io::ErrorKind::FileTooLarge => State::ByteLimit {
                        allowed: CONVERSATION_IMAGE_BYTES,
                    },
                    Err(_) => State::Unavailable,
                },
            )
        })
        .await
        .map_err(io::Error::other)??;
        let current = self.question_presentation(&target, user, false).await?;
        if !matches!(current, Ok((current, _)) if current.commands.same_channel(&worker.commands))
            || worker.is_stopping()
            || worker.is_stopped()
        {
            return Ok(State::Retired);
        }
        Ok(result)
    }
}
