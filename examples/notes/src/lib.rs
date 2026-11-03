pub mod device;
pub mod model;

pub use device::{Device, NotesError, Result, Step, Synced};
pub use model::{Note, Notes, Stamp, merge};
