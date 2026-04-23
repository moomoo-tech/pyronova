//! Network-layer primitives: TCP listener setup, accept-error
//! handling, platform-specific socket options.
//!
//! Currently just `listener`. Keeping the module scoped thin rather
//! than inventing a bigger `server::{topology, pipeline, listener}`
//! tree — TPC topology lives at `crate::tpc`, and adding a pipeline/
//! middleware layer is on the "discuss later" list (item 5 from the
//! architectural proposals). When those land, they move in here.

pub(crate) mod listener;
