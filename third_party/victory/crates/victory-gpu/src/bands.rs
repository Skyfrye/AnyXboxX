//! The priority bands of the process's threads, from the top:
//!
//! 1. ours that a frame waits for (the submitting thread, the relay, the
//!    list threads), at the priorities they give themselves;
//! 2. the title's, in the title's own order, but none below
//!    [`TITLE_FLOOR`] ([`title`]);
//! 3. our background work, at [`BACKGROUND`], below every thread of the
//!    title: the pipeline workers (but the two that make passes over the
//!    screen, which a draw waits for), the pipeline library's saves and its
//!    trim, the texture store. The D3D runtime runs its own background
//!    threads, the shaders the driver makes again, there too
//!    (`background.rs`).
//!
//! A process gives its threads seven priorities (idle, lowest, below normal,
//! normal, above normal, highest, time critical). Our background takes the
//! lowest, so the title's idle threads share the next one with its lowest
//! ones; above that the title's order is its own.
//!
//! Before, the default set (`scripts/play.ps1`, `VICTORY_TITLE_PRIORITIES=0`)
//! left the title's six idle threads, the streaming pager's among them, at
//! idle, below ten pipeline workers at lowest (two thirds of 16 logical
//! processors) and two more at below normal. In a cold run on an 8-core,
//! 16-thread processor (audit threads CPU-2, run nb1: the machine at 97-100 %,
//! the pipeline workers 8.8 cores) the busiest thread of the title had 1.7 s
//! of every 10 while pipelines were made, and 9.3 s within one sample of the
//! workers running out of work.

use std::marker::PhantomData;

use windows_sys::Win32::System::Threading::{
    GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_IDLE,
    THREAD_PRIORITY_LOWEST,
};

/// The priority of our background work.
pub const BACKGROUND: i32 = THREAD_PRIORITY_IDLE;
/// The lowest priority a thread of the title runs at.
pub const TITLE_FLOOR: i32 = THREAD_PRIORITY_LOWEST;

/// The priority a thread of the title runs at when the title asks for
/// `asked`: that, but not below [`TITLE_FLOOR`]. The order is kept: a thread
/// the title puts above another is not below it here (idle and lowest are
/// one). The background modes (`THREAD_MODE_BACKGROUND_BEGIN` and `_END`)
/// pass as they are.
pub fn title(asked: i32) -> i32 {
    asked.max(TITLE_FLOOR)
}

/// Puts the calling thread in the background band; `what` names it in the
/// log should the system refuse.
pub(crate) fn enter_background(what: &str) {
    // SAFETY: the calling thread's own priority.
    if unsafe { SetThreadPriority(GetCurrentThread(), BACKGROUND) } == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "a background thread of ours runs above the background band");
    }
}

/// A thread named `name` that runs `f` in the background band.
pub(crate) fn spawn_background<T: Send + 'static>(
    name: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<T>> {
    let what = name.to_owned();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            enter_background(&what);
            f()
        })
}

/// The calling thread in the background band until this is dropped, then
/// back at its own priority: background work on a thread of another band
/// (a library save on a worker of passes over the screen). Nothing changes
/// on a thread already at or below the band.
pub(crate) struct Lowered {
    was: Option<i32>,
    /// (Of the thread that made it: the priority is that thread's.)
    _here: PhantomData<*const ()>,
}

impl Lowered {
    pub(crate) fn new() -> Self {
        /// `GetThreadPriority`'s failure.
        const ERROR_RETURN: i32 = i32::MAX;
        // SAFETY: the calling thread's own priority.
        let was = unsafe {
            let thread = GetCurrentThread();
            let was = GetThreadPriority(thread);
            (was != ERROR_RETURN && was > BACKGROUND && SetThreadPriority(thread, BACKGROUND) != 0)
                .then_some(was)
        };
        Self {
            was,
            _here: PhantomData,
        }
    }
}

impl Drop for Lowered {
    fn drop(&mut self) {
        if let Some(was) = self.was {
            // SAFETY: as in `new`, on the same thread (`Lowered` is not `Send`).
            unsafe { SetThreadPriority(GetCurrentThread(), was) };
        }
    }
}

/// The calling thread at `to` at least until this is dropped, then back at
/// its own priority: work a frame waits for on a thread of the background
/// band (a pipeline a draw asked for, on a background worker). Nothing
/// changes on a thread already at or above `to`.
pub(crate) struct Raised {
    was: Option<i32>,
    /// (Of the thread that made it: the priority is that thread's.)
    _here: PhantomData<*const ()>,
}

impl Raised {
    pub(crate) fn to(to: i32) -> Self {
        /// `GetThreadPriority`'s failure.
        const ERROR_RETURN: i32 = i32::MAX;
        // SAFETY: the calling thread's own priority.
        let was = unsafe {
            let thread = GetCurrentThread();
            let was = GetThreadPriority(thread);
            (was != ERROR_RETURN && was < to && SetThreadPriority(thread, to) != 0).then_some(was)
        };
        Self {
            was,
            _here: PhantomData,
        }
    }
}

impl Drop for Raised {
    fn drop(&mut self) {
        if let Some(was) = self.was {
            // SAFETY: as in `to`, on the same thread (`Raised` is not `Send`).
            unsafe { SetThreadPriority(GetCurrentThread(), was) };
        }
    }
}

/// Threads of ours a frame waits for that keep a core busy, whatever the
/// background does: the submitting thread, the relay, and the list threads'
/// work (eight threads, about one and a half cores together in the world:
/// 143 ms of their time in a frame of 91-100 ms, run nbc-measure).
pub(crate) const FRAME_CRITICAL: usize = 3;

/// The threads of a background pool on a processor of `physical` cores: the
/// cores [`FRAME_CRITICAL`] leaves, 2 to 16. Cores, not logical processors:
/// two of these on one core share its units, and every one of our background
/// threads beyond the free cores runs only to be put aside again, holding
/// what it holds (the driver's locks while it makes a pipeline).
pub(crate) fn pool_for(physical: usize) -> usize {
    physical.saturating_sub(FRAME_CRITICAL).clamp(2, 16)
}

/// The physical cores [`pool_for`] goes by: the system's count, else half
/// the logical processors.
pub(crate) fn physical_cores() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| {
        crate::cores::physical_cores().unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(2, |n| n.get())
                .div_ceil(2)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::THREAD_PRIORITY_BELOW_NORMAL;

    fn priority() -> i32 {
        // SAFETY: a plain query of this thread.
        unsafe { GetThreadPriority(GetCurrentThread()) }
    }

    #[test]
    fn every_thread_of_the_title_runs_above_our_background_in_its_own_order() {
        // All a process can ask for (and the ones only a real-time process
        // can, -7 to -3 and 3 to 6).
        let asked = [-15, -7, -3, -2, -1, 0, 1, 2, 3, 6, 15];
        let given: Vec<i32> = asked.iter().map(|&p| title(p)).collect();
        assert_eq!(given, [-2, -2, -2, -2, -1, 0, 1, 2, 3, 6, 15]);
        assert!(given.iter().all(|&p| p > BACKGROUND));
        assert!(given.windows(2).all(|w| w[0] <= w[1]));
        // The background modes are no priorities.
        assert_eq!((title(0x1_0000), title(0x2_0000)), (0x1_0000, 0x2_0000));
    }

    #[test]
    fn background_threads_and_stretches_run_in_the_background_band() {
        let inside = spawn_background("victory-test-background", priority)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(inside, BACKGROUND);
        std::thread::spawn(|| {
            for start in [0, THREAD_PRIORITY_BELOW_NORMAL, BACKGROUND] {
                // SAFETY: this thread's own priority.
                unsafe { SetThreadPriority(GetCurrentThread(), start) };
                {
                    let _lowered = Lowered::new();
                    assert_eq!(priority(), BACKGROUND);
                }
                assert_eq!(priority(), start, "back where it was");
            }
        })
        .join()
        .unwrap();
    }

    /// Work a frame waits for on a background thread runs raised, then the
    /// thread goes back to the band; a thread already above stays as it is.
    #[test]
    fn a_stretch_a_frame_waits_for_runs_raised() {
        let seen = spawn_background("victory-test-raised", || {
            let inside = {
                let _raised = Raised::to(THREAD_PRIORITY_BELOW_NORMAL);
                priority()
            };
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), 0) };
            let above = {
                let _raised = Raised::to(THREAD_PRIORITY_BELOW_NORMAL);
                priority()
            };
            (inside, above, priority())
        })
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(seen, (THREAD_PRIORITY_BELOW_NORMAL, 0, 0));
        let after = spawn_background("victory-test-raised-back", || {
            drop(Raised::to(THREAD_PRIORITY_BELOW_NORMAL));
            priority()
        })
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(after, BACKGROUND, "back in the band");
    }

    #[test]
    fn background_pools_follow_the_cores_a_frame_leaves() {
        // This machine's 8 cores (16 logical processors): 5, not 10 + 2.
        assert_eq!(pool_for(8), 5);
        assert_eq!(
            [pool_for(0), pool_for(1), pool_for(4), pool_for(6)],
            [2, 2, 2, 3]
        );
        assert_eq!(
            [pool_for(12), pool_for(16), pool_for(24), pool_for(64)],
            [9, 13, 16, 16]
        );
        // Counted once per core, a core having one or two logical processors.
        let logical = std::thread::available_parallelism().unwrap().get();
        let cores = crate::cores::physical_cores().expect("the system says");
        assert!(
            cores <= logical && cores * 2 >= logical,
            "{cores} cores, {logical} logical processors"
        );
        assert_eq!(physical_cores(), cores);
    }
}
