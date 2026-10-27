//! DupeMap core: manifest import, the duplicate index and the queries the app is built on.
//!
//! * [`manifest`] reads and validates TSV manifests (`hash<TAB>size<TAB>host<TAB>path`).
//! * [`db`] stores them in SQLite (one source per host, a folder tree, file rows).
//! * [`index`] builds the in-memory duplicate index: copy counts, folder statistics, listings.
//! * [`summary`] totals by file type (editable categories of extensions) and by source.
//! * [`synth`] generates realistic synthetic manifests for tests and benchmarks.

pub mod db;
pub mod error;
pub mod ignore;
pub mod index;
pub mod manifest;
pub mod natsort;
pub mod paths;
pub mod report;
pub mod summary;
pub mod synth;

pub use db::Db;
pub use error::{Error, Result};
pub use index::Index;
pub use report::{ImportReport, Severity};
