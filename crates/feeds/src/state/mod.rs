//! Shared request state.
//!
//! One cloneable struct handed to every handler. It holds a connection pool and read
//! only configuration; anything mutable (a verification cache, rate-limit buckets)
//! arrives later behind an explicit interior-mutability wrapper, so it is obvious
//! which parts of the state are shared beyond a single request.
//!
//! The directory splits that state by layer: [`app`] is the cloneable struct the
//! handlers receive, [`identity`] is the verified-token vocabulary and the
//! process-local map behind it, and [`shared`] layers that map in front of the
//! cross-replica cache. Everything a caller used to name as `state::X` is
//! re-exported here, so no call site had to move.

mod app;
mod identity;
mod shared;

pub use app::AppState;
pub use identity::{Identity, VerificationCache};
pub use shared::SharedIdentityCache;

#[cfg(test)]
mod tests;
