use crate::core::model::{FileId, UrlId};
use crate::core::reducer::CoreEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreCommand {
    SubmitUrl { url: UrlId },
    DeleteFile { file_id: FileId },
    RetryFile { file_id: FileId },
    ResetFile { file_id: FileId },
}

impl CoreCommand {
    #[must_use]
    pub fn into_event(self) -> CoreEvent {
        match self {
            Self::SubmitUrl { url } => CoreEvent::UrlSubmitted { url },
            Self::DeleteFile { file_id } => CoreEvent::FileDeleted { file_id },
            Self::RetryFile { file_id } => CoreEvent::FileRetryRequested { file_id },
            Self::ResetFile { file_id } => CoreEvent::FileResetRequested { file_id },
        }
    }
}
