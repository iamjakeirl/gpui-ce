use std::{
    cell::Cell,
    ffi::c_void,
    ptr::NonNull,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{ThreadId, current},
    time::Duration,
};

use anyhow::Context;
use gpui_util::ResultExt;
use parking_lot::RwLock;
use windows::{
    Win32::{
        Foundation::{FILETIME, LPARAM, LRESULT, WPARAM},
        Media::{timeBeginPeriod, timeEndPeriod},
        System::Threading::{
            CloseThreadpoolTimer, CreateThreadpoolTimer, GetCurrentThread, PTP_CALLBACK_INSTANCE,
            PTP_TIMER, SetThreadPriority, SetThreadpoolTimer, THREAD_PRIORITY_TIME_CRITICAL,
            TP_CALLBACK_ENVIRON_V3, TP_CALLBACK_PRIORITY, TP_CALLBACK_PRIORITY_HIGH,
            TP_CALLBACK_PRIORITY_LOW, TP_CALLBACK_PRIORITY_NORMAL, TrySubmitThreadpoolCallback,
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, GWLP_USERDATA, HWND_MESSAGE, PostMessageW,
            RegisterClassW, RegisterWindowMessageW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
        },
    },
    core::{PCWSTR, w},
};

use crate::{
    HWND, SafeHwnd, WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD, get_module_handle, get_window_long,
    set_window_long,
};
use gpui::{
    PlatformDispatcher, Priority, PriorityQueueSender, RunnableVariant, TimerResolutionGuard,
};

pub(crate) struct WindowsDispatcher {
    pub(crate) wake_posted: AtomicBool,
    main_sender: PriorityQueueSender<RunnableVariant>,
    main_thread_id: ThreadId,
    pub(crate) platform_window_handle: SafeHwnd,
    validation_number: usize,
    /// Whether the platform takes main-thread work: false once it has ended. Dispatching holds
    /// it for reading, so once [`Self::close`] returns, nothing more reaches the queue.
    open: RwLock<bool>,
    /// An embedded platform's thread's [`reaper`].
    reaper: Option<Reaper>,
}

impl WindowsDispatcher {
    pub(crate) fn new(
        main_sender: PriorityQueueSender<RunnableVariant>,
        platform_window_handle: HWND,
        validation_number: usize,
        reaper: Option<Reaper>,
    ) -> Self {
        let main_thread_id = current().id();
        let platform_window_handle = platform_window_handle.into();

        WindowsDispatcher {
            main_sender,
            main_thread_id,
            platform_window_handle,
            validation_number,
            open: RwLock::new(true),
            reaper,
            wake_posted: AtomicBool::new(false),
        }
    }

    fn dispatch_on_threadpool(&self, priority: TP_CALLBACK_PRIORITY, runnable: RunnableVariant) {
        let environ = TP_CALLBACK_ENVIRON_V3 {
            Version: 3,
            CallbackPriority: priority,
            Size: size_of::<TP_CALLBACK_ENVIRON_V3>() as u32,
            ..Default::default()
        };

        // If the thread pool never runs our callback, the matching `from_raw` is never called, which leaks the runnable.
        // Dropping the scheduled runnable would cancel its task and make the next poll of any awaiter panic. Since we expect
        // the scenario to usually happen during shutdown, this leak is acceptable.
        let context = runnable.into_raw().as_ptr() as *mut c_void;

        unsafe {
            TrySubmitThreadpoolCallback(Some(run_work_callback), Some(context), Some(&environ))
                .log_err();
        }
    }

    fn dispatch_on_threadpool_after(&self, runnable: RunnableVariant, duration: Duration) {
        let context = runnable.into_raw().as_ptr() as *mut c_void;

        unsafe {
            if let Ok(timer) = CreateThreadpoolTimer(Some(run_timer_callback), Some(context), None)
            {
                // Negative FILETIME expresses a relative delay in 100ns ticks
                let ticks = (duration.as_nanos() / 100).min(i64::MAX as u128) as i64;
                let due = (-ticks) as u64;
                let due_time = FILETIME {
                    dwLowDateTime: due as u32,
                    dwHighDateTime: (due >> 32) as u32,
                };
                SetThreadpoolTimer(timer, Some(&due_time), 0, None);
            }
        }
    }

    /// Takes no more main-thread work: runnables dispatched from now on are reaped
    /// ([`Self::reap`]). The caller deals with those already queued.
    pub(crate) fn close(&self) {
        *self.open.write() = false;
    }

    /// Disposes of a runnable that the platform will never run (it has ended, or a panic
    /// poisoned it).
    pub(crate) fn reap(&self, runnable: RunnableVariant) {
        // NOTE: Runnable may wrap a Future that is !Send.
        //
        // This is usually safe because we only poll it on the main thread.
        // However if the send fails, we know that:
        // 1. main_receiver has been dropped (which implies the app is shutting down)
        // 2. we are on a background thread.
        // It is not safe to drop something !Send on the wrong thread, and
        // the app will exit soon anyway, so we must forget the runnable.
        //
        // An embedded platform's thread is the host's, which lives on and may start GPUI
        // again: its reaper drops the runnable there instead.
        let message = reaper_message();
        match self.reaper {
            Some(reaper) if message != 0 => {
                let runnable = runnable.into_raw().as_ptr();
                // If the post fails (the thread has ended, or its queue is full), the
                // runnable leaks: no other thread may drop it.
                unsafe {
                    PostMessageW(
                        Some(reaper.window.as_raw()),
                        message,
                        WPARAM(reaper.generation),
                        LPARAM(runnable as isize),
                    )
                    .log_err();
                }
            }
            _ => std::mem::forget(runnable),
        }
    }

    #[inline(always)]
    pub(crate) fn execute_runnable(runnable: RunnableVariant) {
        let location = runnable.metadata().location;
        let spawned = runnable.metadata().spawned;
        gpui::profiler::update_running_task(spawned, location);
        runnable.run();
        gpui::profiler::save_task_timing();
    }
}

impl PlatformDispatcher for WindowsDispatcher {
    fn is_main_thread(&self) -> bool {
        current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        let priority = match priority {
            Priority::RealtimeAudio => {
                panic!("RealtimeAudio priority should use spawn_realtime, not dispatch")
            }
            Priority::High => TP_CALLBACK_PRIORITY_HIGH,
            Priority::Medium => TP_CALLBACK_PRIORITY_NORMAL,
            Priority::Low => TP_CALLBACK_PRIORITY_LOW,
        };
        self.dispatch_on_threadpool(priority, runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        let open = self.open.read();
        if !*open {
            drop(open);
            return self.reap(runnable);
        }
        match self.main_sender.send(priority, runnable) {
            Ok(_) => {
                if !self.wake_posted.swap(true, Ordering::AcqRel) {
                    unsafe {
                        PostMessageW(
                            Some(self.platform_window_handle.as_raw()),
                            WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD,
                            WPARAM(self.validation_number),
                            LPARAM(0),
                        )
                        .log_err();
                    }
                }
            }
            Err(runnable) => {
                drop(open);
                self.reap(runnable.0);
            }
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        self.dispatch_on_threadpool_after(runnable, duration);
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            // SAFETY: always safe to call
            let thread_handle = unsafe { GetCurrentThread() };

            // SAFETY: thread_handle is a valid handle to the current thread
            unsafe { SetThreadPriority(thread_handle, THREAD_PRIORITY_TIME_CRITICAL) }
                .context("thread priority")
                .log_err();

            f();
        });
    }

    fn increase_timer_resolution(&self) -> TimerResolutionGuard {
        unsafe {
            timeBeginPeriod(1);
        }
        gpui_util::defer(Box::new(|| unsafe {
            timeEndPeriod(1);
        }))
    }
}

unsafe extern "system" fn run_work_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
) {
    let runnable = unsafe { RunnableVariant::from_raw(NonNull::new_unchecked(context as *mut ())) };
    crate::guard_callback(|| (), || WindowsDispatcher::execute_runnable(runnable));
}

unsafe extern "system" fn run_timer_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    timer: PTP_TIMER,
) {
    let runnable = unsafe { RunnableVariant::from_raw(NonNull::new_unchecked(context as *mut ())) };
    crate::guard_callback(|| (), || WindowsDispatcher::execute_runnable(runnable));
    unsafe { CloseThreadpoolTimer(timer) };
}

/// A thread's reaper (see [`reaper`]): its window, and a number no other reaper has.
#[derive(Clone, Copy)]
pub(crate) struct Reaper {
    window: SafeHwnd,
    generation: usize,
}

/// This thread's reaper: a message-only window that drops the runnables woken for an embedded
/// platform after it ended (a timer that fired late, say), on this thread, where their `!Send`
/// futures belong. Created once per thread, it stays for the thread's life; the embedded
/// platform's module is pinned, so its window procedure does too.
pub(crate) fn reaper() -> anyhow::Result<Reaper> {
    const CLASS_NAME: PCWSTR = w!("Zed::Reaper");
    static GENERATIONS: AtomicUsize = AtomicUsize::new(1);
    thread_local! {
        static REAPER: Cell<Option<Reaper>> = const { Cell::new(None) };
    }
    if let Some(reaper) = REAPER.get() {
        return Ok(reaper);
    }
    if reaper_message() == 0 {
        anyhow::bail!("registering the reaper's message failed");
    }
    // Registered for this module, like the platform's window class.
    let class = WNDCLASSW {
        lpfnWndProc: Some(reaper_procedure),
        lpszClassName: CLASS_NAME,
        hInstance: get_module_handle().into(),
        ..Default::default()
    };
    unsafe { RegisterClassW(&class) };
    let window: SafeHwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(get_module_handle().into()),
            None,
        )
    }
    .context("creating the reaper window")?
    .into();
    let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
    unsafe { set_window_long(window.as_raw(), GWLP_USERDATA, generation as isize) };
    let reaper = Reaper { window, generation };
    REAPER.set(Some(reaper));
    Ok(reaper)
}

/// The reaper's message (0 if registering failed). Its name is unique to this module, which
/// is pinned, so other windows, including other modules' reapers, ignore it. First called on
/// the reaper's thread, so no thread pool thread registers it.
fn reaper_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| {
        let name = format!("GPUI reaper {:x}", get_module_handle().0 as usize);
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
    })
}

unsafe extern "system" fn reaper_procedure(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg != 0 && msg == reaper_message() {
        // `WindowsDispatcher::reap` posted a runnable's raw pointer (`into_raw`), once, to the
        // reaper with this generation, on the runnable's thread. A window that took over a
        // recycled handle (another thread's reaper) leaves it alone: it leaks, since dropping
        // it on the wrong thread would abort.
        if wparam.0 == unsafe { get_window_long(hwnd, GWLP_USERDATA) } as usize {
            let runnable =
                unsafe { RunnableVariant::from_raw(NonNull::new_unchecked(lparam.0 as *mut ())) };
            // Cancels its task and drops its future here.
            drop(runnable);
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
