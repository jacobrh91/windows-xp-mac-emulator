//! Sound and video: the emulated sound card and multimedia timers (`audio`), the Mac's
//! speakers they play through (`speaker`), Bink movies decoded natively (`bink`) and Miles'
//! MP3 codec replaced by a native decoder (`miles_mp3`).

pub mod audio;
pub mod bink;
pub mod miles_mp3;
pub mod speaker;
