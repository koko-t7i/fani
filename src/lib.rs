mod adapters;
pub mod application;
mod cli;
mod composition;
mod diagnostics;
pub mod domain;

pub use adapters::process::run_process_wrapper_if_requested;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run_cli() -> i32 {
    cli::run()
}

#[doc(hidden)]
pub mod test_support {
    pub mod db {
        pub use crate::adapters::db::Database;
        pub use crate::application::ports::{
            AttemptCandidateInput, AttemptInput, CanonicalFileInput, CanonicalTranslationInput,
            OutboxKind, PublicationManifestFile, PublicationManifestInput, TrustTranslationInput,
        };
    }

    pub mod github {
        pub use crate::adapters::github::{
            EnsurePullRequest, GhClient, ReconcileAction, locale_branch,
        };
    }

    pub mod gitout {
        pub use crate::adapters::gitout::{
            CandidateCommit, ChangeKind, PathChange, create_candidate,
            create_candidate_from_contents, push_candidate,
        };
    }
}
