mod file_classification;
mod snapshot;

pub(crate) use file_classification::is_textish;
pub(crate) use snapshot::{remote_content_addressed_uri, FileMeta, RepoSnapshot};
