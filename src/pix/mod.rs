//! PIX integration utilities
//!
//! External PIX operations are driven through `pixtool.exe` (see [`pixtool`]).
//! In-process `pix3.h` capture/marker APIs are intentionally not used: those
//! only affect the process that loads the capturer, so they cannot instrument
//! a separate target application from this server.

pub mod pixtool;

pub use pixtool::PixTool;
