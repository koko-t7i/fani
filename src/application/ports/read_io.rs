//! Read-only I/O capabilities available to preview planning.
use crate::application::settings::RepoConfig;
use crate::domain::model::SourceDocument;
use anyhow::Result;
use std::path::Path;

pub trait SourceReader {
    fn resolve_source_revision(&self, repo: &RepoConfig) -> Result<String>;
    fn discover(&self, repo: &RepoConfig, source_revision: &str) -> Result<Vec<SourceDocument>>;
}

pub trait TargetReader {
    fn read(&self, root: &Path, relative: &Path) -> Result<Option<Vec<u8>>>;
}
