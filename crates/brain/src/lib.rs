//! NevoFlux knowledge-base abstraction crate.
//!
//! Defines the [`BrainEngine`] trait and supporting types that the rest of
//! the workspace will use to talk to a knowledge-base backend. v1 has no
//! concrete `BrainEngine` impl — M3 will introduce `GbrainEngine` wrapping
//! the gbrain subprocess.
//!
//! Also defines the Path A hook scaffold ([`IngestSignal`],
//! [`EnrichmentProvider`]) — v1 only ships [`ManualSaveIngest`] and
//! [`NoOpEnrichmentProvider`]; v2 fleshes out the rest.
//!
//! See `docs/plans/2026-05-24-knowledge-base-spike-plan.md` 附录 B for
//! spike findings that shaped this surface (e.g., gbrain's --brain-dir
//! flag is ignored, model name remap goes through llm-gateway, etc.).

#![deny(rust_2018_idioms)]
#![warn(missing_docs)]

pub mod engine;
pub mod enrichment;
pub mod error;
pub mod ingest;
pub mod types;

pub use engine::BrainEngine;
pub use enrichment::{
    EnrichmentProvider, EnrichmentResult, EntityKind, EntityRef, NoOpEnrichmentProvider,
};
pub use error::{BrainError, BrainResult};
pub use ingest::{
    BrainSignal, IngestAction, IngestResult, IngestSignal, ManualSaveIngest, SaveContent,
    SaveSource,
};
pub use types::{
    BrainPage, Hit, ImportOpts, ImportReport, ImportTrust, NbrainBundle, PageMeta, PutResult,
    SearchOpts, Selection, SourceMeta, SourceSpec, StripRules, SyncReport,
};
