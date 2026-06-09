mod core;
mod load;
mod schema;

pub use core::*;
pub use load::*;
pub use schema::*;

// Internal helpers used by other modules in the crate (e.g. new_project's
// workspace append goes through the same atomic-write path the config loader
// uses for instance-id writeback).
pub(crate) use load::atomic_write_with_lock;

#[cfg(test)]
#[path = "tests.rs"]
mod tests_file;
