//! Structural extraction of YouTube renderers from InnerTube payloads.
//!
//! InnerTube payloads are deeply nested and YouTube reshuffles them without
//! notice, so rather than mirroring the exact tree these parsers search it
//! structurally — "find the first node that looks like a video renderer"
//! survives far more YouTube redesigns than a hard-coded path would. This is
//! a 1:1 port of `youtube/parsers/*.ts`.

pub mod channel;
pub mod comments;
pub mod renderers;
pub mod search;
pub mod transcript;
pub mod util;
pub mod video;
