//! The gRPC services, one module per service in `proto/`. Handlers convert
//! between the generated messages and the daemon's own types (in
//! `convert`) and end in `.map_err(crate::error::status)`.

pub mod daemon;
pub mod maintenance;
pub mod repositories;
pub mod stores;
pub mod workspaces;

mod convert;
