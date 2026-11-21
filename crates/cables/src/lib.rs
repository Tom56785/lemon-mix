//! Virtual audio cables exported as USB Audio Class 2.0 devices over USB/IP,
//! so that Windows' in-box `usbaudio2.sys` exposes each one as standard
//! playback and/or recording endpoints.
//!
//! This crate holds the transport and the device: the USB/IP server, the UAC2
//! descriptors and control requests, the bus frame clock, the frame-indexed
//! delay line, and attaching the devices with usbip-win2. What a cable does
//! with its audio is left to a [`uac2::CableAudio`] implementation: on its own
//! a cable loops its playback side back to its recording side through a
//! [`audio::DelayLine`]; a mixer can route it instead.

pub mod audio;
pub mod event;
pub mod logging;
pub mod uac2;
pub mod usbip;

#[cfg(windows)]
pub mod attach;
#[cfg(windows)]
mod rt;
#[cfg(windows)]
pub mod win;

#[cfg(windows)]
pub use rt::{AudioThread, enter_audio_thread, raise_process_priority};

#[cfg(not(windows))]
mod rt_other {
    use crate::logging::Logger;

    pub fn raise_process_priority(_: &Logger) {}

    /// Stands in for the multimedia scheduler registration outside Windows.
    pub struct AudioThread;

    pub fn enter_audio_thread(_: &Logger) -> AudioThread {
        AudioThread
    }
}
#[cfg(not(windows))]
pub use rt_other::{AudioThread, enter_audio_thread, raise_process_priority};

#[cfg(test)]
pub(crate) mod alloc_counter {
    //! Counts heap allocations made by the current thread.

    // The initializer below is already const; clippy misreports it.
    #![allow(clippy::missing_const_for_thread_local)]

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
    }

    struct Counting;

    fn count() {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }

    // SAFETY: every call is forwarded unchanged to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    pub fn allocations() -> u64 {
        COUNT.with(|c| c.get())
    }
}
