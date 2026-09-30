//! Compares Overpass API database directories, treating two databases as
//! equal when they differ only in bytes Overpass never reads.

pub mod compare;
pub mod format;
pub mod groups;
pub mod lz4;

pub use compare::{
    block_layout, compare_dirs, BlockLayout, CmpError, FileReport, Ignored, Outcome,
};
pub use groups::{index_groups, Group};
