//! On-demand reads for images owned by one committed conversation part.

use crate::attachment_store::read_retained_image;

use super::*;

const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

pub struct ConversationImageRequest {
    pub source: SourceSelection,
    pub head: HistoryHead,
    pub item: ConversationItemId,
    pub part: u32,
}

pub enum ConversationImageResult {
    Ready { media_type: String, bytes: Vec<u8> },
    Capacity(ConversationCapacity),
    ByteLimit { allowed: usize },
    SourceChanged,
    ContentChanged,
    Unavailable,
}

impl SessionCatalog {
    pub async fn read_conversation_image(
        &self,
        request: ConversationImageRequest,
        cache: Arc<ConversationArtifactSources>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<ConversationImageResult> {
        use ConversationImageResult as Reply;

        let flow = request.source.flow.clone();
        authorize(&flow)?;
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(Reply::Capacity(ConversationCapacity {
                kind: ConversationCapacityKind::ReadersBusy,
                required_at_least: MAX_READERS + 1,
                allowed: MAX_READERS,
                restart_required: false,
            }));
        };
        let permit = Arc::new(permit);
        let root = self.recorded_root()?;
        let observation = Observation::default();
        let mut source = match self
            .prepare_record_source(
                &cache,
                &root,
                &request.source,
                &request.head,
                &permit,
                &observation,
            )
            .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(capacity)) => return Ok(Reply::Capacity(capacity)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(Reply::SourceChanged);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = authorize(&flow) {
            crate::read_work::retire((source, Arc::clone(&permit)));
            return Err(error);
        }
        let control = observation.0.read_control();
        let result = read_step(&permit, move || {
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            let result = match source.read_image_record(request.item, request.part) {
                Ok(Ok(image)) => match read_retained_image(&image, MAX_IMAGE_BYTES, &control) {
                    Ok(bytes) => Reply::Ready {
                        media_type: image.media_type,
                        bytes,
                    },
                    Err(error) => match error.kind() {
                        io::ErrorKind::FileTooLarge => Reply::ByteLimit {
                            allowed: MAX_IMAGE_BYTES,
                        },
                        io::ErrorKind::InvalidData => Reply::ContentChanged,
                        io::ErrorKind::Interrupted => return Err(error),
                        _ => Reply::Unavailable,
                    },
                },
                Ok(Err(capacity)) => Reply::Capacity(capacity),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Reply::Unavailable,
                Err(error) => return Err(error),
            };
            if !source.storage_is_current() {
                return Ok(Reply::SourceChanged);
            }
            cache.retain(root, source);
            Ok(result)
        })
        .await?;
        authorize(&flow)?;
        Ok(result)
    }
}
