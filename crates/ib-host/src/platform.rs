//! Small OS integrations for the host process.

use ib_geometry::Region;

/// Makes desktop coordinates physical pixels on every monitor. Must run
/// before any window or input API is used.
pub fn init_process() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        // SAFETY: plain FFI call; failure (already set) is harmless.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            tracing::debug!("DPI awareness already set: {e}");
        }
    }
}

/// Gives the pen thread priority over normal work so injection isn't
/// delayed by background load.
pub fn raise_thread_priority() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
        };
        // SAFETY: GetCurrentThread returns a pseudo-handle valid for this call.
        if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
            tracing::warn!("could not raise pen thread priority: {e}");
        }
    }
}

/// Bounds of the primary display in physical pixels.
pub fn primary_display() -> Region {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        // SAFETY: plain FFI calls without pointers.
        let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        if let (Ok(width), Ok(height)) = (u32::try_from(w), u32::try_from(h))
            && width > 0
            && height > 0
        {
            return Region {
                x: 0,
                y: 0,
                width,
                height,
            };
        }
    }
    Region {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }
}
