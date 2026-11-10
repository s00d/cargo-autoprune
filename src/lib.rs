//! cargo-autoprune: a `rustc-wrapper` that keeps Cargo `target/` dirs from growing.
//!
//! As a wrapper (`build.rustc-wrapper = "cargo-autoprune"`) it costs one `stat` per compiled crate,
//! then `exec`s rustc (or the chained wrapper from `CARGO_AUTOPRUNE_WRAPPER`). At most once per
//! interval it spawns a detached copy of itself that waits for cargo to release the build locks
//! and prunes superseded artifacts of that profile dir (see [`plan`] for the rules).

pub mod cli;
mod fingerprint;
mod fsutil;
pub mod layout;
mod lock;
mod plan;
pub mod prune;
mod scan;
pub(crate) mod state;
pub mod wrapper;

#[cfg(test)]
mod testutil;

pub use fsutil::{human, parse_size};
