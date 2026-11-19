//! File-system helpers shared by all crates (naming, validation, safe creation, placeholder flags).

#![warn(missing_docs)]

pub mod attrs;
pub mod create;
pub mod fold;
pub mod name;

pub use create::{create_new_dir, create_new_file};
pub use fold::{fold, fold_into};
pub use name::{NameError, unique_name, validate_name};
