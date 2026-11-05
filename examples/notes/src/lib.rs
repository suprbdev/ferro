//! Offline-first notes kept per device in a ferro database and converged by sync.
//!
//! Each [`Device`] is a directory named for the device, holding its own database of [`Notes`].
//! Every change is stamped with a Lamport clock and the device name, so the text, each tag, and the
//! deleted flag of a [`Note`] carry the [`Stamp`] of their last change. [`Device::sync`] pulls the
//! other device's commits with `pull`, merging concurrent edits with [`merge`], which keeps the
//! newer stamp of each part, and then pulls back the other way so both devices end on the same
//! state. Only the objects one side lacks cross between them. Notes are indexed by each tag. The
//! `notes` and `notes-scale` binaries and the tests use this library.
//!
//! ```sh
//! make notes ARGS='--device phone add "buy milk" --tag shopping'
//! make notes ARGS='--device laptop sync phone'
//! ```

pub mod device;
pub mod model;

pub use device::{Device, NotesError, Result, Step, Synced};
pub use model::{Note, Notes, Stamp, merge};
