pub mod chunk;
pub mod cli;
pub mod commands;
pub mod index;
pub mod merge;
pub mod merge3;
pub mod model;
pub mod ops;
pub mod query;
pub mod render;
pub mod synth;
pub mod text;

use std::error::Error;

pub type Res<T> = Result<T, Box<dyn Error>>;
