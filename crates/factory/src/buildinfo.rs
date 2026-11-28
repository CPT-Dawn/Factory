//! The commit this binary was built from, as build.rs names it.

/// The short sha of the build, with `-dirty` when its tree had uncommitted edits, or `unknown`.
pub const BUILD: &str = env!("FACTORY_BUILD");

/// What `factory --version` prints after the name: `0.1.0 (ed05528)`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FACTORY_BUILD"), ")");
