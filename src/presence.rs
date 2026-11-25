//! Whether you are at the machine. Herdr's focus says which pane you would be
//! looking at, not that you are looking: a focused pane left overnight is not
//! attention.

/// Seconds since the last keyboard, mouse or trackpad input anywhere in the
/// login session. Needs no permission prompt.
#[cfg(target_os = "macos")]
pub fn input_idle_seconds() -> i64 {
    // kCGEventSourceStateHIDSystemState: input as the hardware delivered it.
    const HID_SYSTEM_STATE: i32 = 1;
    // kCGAnyInputEventType.
    const ANY_INPUT: u32 = !0;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, ANY_INPUT) as i64 }
}
