//! The Windows desktop: window manager and USER32/GDI32 on top of it, the software
//! framebuffer they draw into, and the macOS window that shows the result.

pub mod gfx;
pub mod present;
pub mod user32;
pub mod wm;
