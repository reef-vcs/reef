//! The gRPC services, one module per service in `reef-api`'s `proto/`.
//! Handlers convert between the generated messages and the daemon's own
//! types (in `convert`) and end in `.map_err(crate::error::status)`; the RPCs
//! whose ticket has not landed answer [`stubs::stub`].

pub mod changes;
pub mod daemon;
pub mod maintenance;
pub mod operations;
pub mod presence;
pub mod queries;
pub mod remote;
pub mod repositories;
pub mod stores;
pub mod stubs;
pub mod symbols;
pub mod trees;
pub mod workspaces;

mod convert;
