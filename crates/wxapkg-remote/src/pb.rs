// WeChat: RSCompanyCEO
// Telegram: ryf5584
// GitHub: https://github.com/ryf5584

//! Protobuf messages generated from the staged protocol definitions.
//!
//! Every staged file declares `package ilink`, so prost emits one flat module.

include!(concat!(env!("OUT_DIR"), "/ilink.rs"));
