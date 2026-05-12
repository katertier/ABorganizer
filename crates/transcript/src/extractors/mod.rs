//! Built-in [`crate::Extractor`] impls.
//!
//! Each extractor reads a transcript and produces
//! [`crate::Candidate`] rows. Extractors are stateless +
//! pure-text; the wrapping stage
//! ([`crate::extract_stage::RunExtractorsStage`]) handles DB I/O.
//!
//! Adding a new extractor:
//!
//! 1. Implement [`crate::Extractor`] in a sibling submodule.
//! 2. Register it in [`built_in_extractors`].
//! 3. Reuse an existing field name (`title`, `author`,
//!    `narrator`, `publisher`, `language`) or coordinate a new
//!    one with the consensus stage.

pub mod audiologo_text;
pub mod title_author;

use std::sync::Arc;

use crate::Extractor;

/// All extractors shipped with the daemon. Order doesn't matter
/// — every extractor runs over the same transcript and writes
/// independent provenance rows; consensus picks winners.
#[must_use]
pub fn built_in_extractors() -> Vec<Arc<dyn Extractor>> {
    vec![
        Arc::new(title_author::TitleAuthorExtractor::new()),
        Arc::new(audiologo_text::AudiologoTextExtractor::new()),
    ]
}
