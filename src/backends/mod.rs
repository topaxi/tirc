//! Protocol backend implementations.
//!
//! The contract between backends and the core (the
//! [`ChatBackend`](crate::core::backend::ChatBackend) trait,
//! [`BackendInfo`](crate::core::backend::BackendInfo),
//! [`BackendHandle`](crate::core::backend::BackendHandle), and
//! [`spawn`](crate::core::backend::spawn)) lives in [`crate::core::backend`];
//! this module holds the per-protocol implementations.

pub mod irc;
pub mod matrix;
pub mod mattermost;
