// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! The iLink transport: MMTLS framing plus the request envelope.
//!
//! `mmtls` is the encrypted channel; `envelope` adds the WPKG/request layer on top of it;
//! `hybrid` seals the first authentication packet; `auth` exchanges a confirmed scan for a
//! session.

pub mod auth;
pub mod envelope;
pub mod hybrid;
pub mod mmtls;
