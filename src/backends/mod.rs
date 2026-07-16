//! Protocol backend implementations.
//!
//! The contract between backends and the core (the [`ChatBackend`] trait,
//! [`BackendInfo`], [`BackendHandle`], and [`spawn`]) lives in
//! [`crate::core::backend`]; this module holds the per-protocol
//! implementations.

pub use crate::core::backend::{
    spawn, BackendHandle, BackendInfo, ChatBackend, CommandReceiver, EventSender,
};

pub mod irc;
pub mod matrix;
pub mod mattermost;
