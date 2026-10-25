//! The core every transport shares: who a login is (identity), the sessions it gets (sessions), and the brake
//! on password guessing (ratelimit). Events go through the mods (crate::mods::ModHost::emit).
//!
//! Named like the Python server's package; inside the crate it is always reached as `crate::core::...` (a bare
//! `core::` would mean the standard `core` crate).

pub mod identity;
pub mod ratelimit;
pub mod sessions;
