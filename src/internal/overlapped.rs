//! Overlapped IOCTL plumbing: one `OVERLAPPED` plus event per call, bounded
//! cancel/drain, and the leak-on-poison protocol used when the kernel may still
//! own a buffer. See [`OverlappedCall`] and [`IoctlBuffers`].

use core::ffi::c_void;
use core::marker::PhantomPinned;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NOT_FOUND,
    ERROR_OPERATION_ABORTED, ERROR_TIMEOUT, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

/// Max wait for a cancelled IRP. On timeout, Drop leaks the event and I/O block.
#[cfg(not(test))]
pub(crate) const DRAIN_TIMEOUT_MS: u32 = 2_000;
/// Test-only: drain-timeout tests assert wall-clock; 2 s would dominate CI.
/// The state machine is timeout-independent.
#[cfg(test)]
pub(crate) const DRAIN_TIMEOUT_MS: u32 = 250;

/// Default bound on a blocking IOCTL wait. On expiry the call is left `pending`
/// and the caller gets `ERROR_TIMEOUT`; the next `reset` / `finish` / Drop drains
/// (and may poison).
pub(crate) const IOCTL_DEFAULT_TIMEOUT_MS: u32 = 5_000;

/// Shorter bound for teardown IOCTLs reached from Drop, so destructors stay
/// bounded. Worst case per target is this plus [`DRAIN_TIMEOUT_MS`].
pub(crate) const IOCTL_TEARDOWN_TIMEOUT_MS: u32 = 1_000;

/// Poll interval for [`OverlappedCall::wait_cancellable`] (notification workers).
pub(crate) const WAIT_POLL_MS: u32 = 250;

/// `STATUS_PENDING` in `OVERLAPPED::Internal` while the IRP is outstanding.
const STATUS_PENDING: usize = 0x0000_0103;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    /// IRP finished; `OVERLAPPED` may be reused or freed.
    Drained,
    /// Timed out; do not free or reuse `OVERLAPPED`.
    TimedOut,
}

/// ViGEmBus device handle; closed when the last `Arc` drops.
pub(crate) struct BusInner {
    pub(crate) handle: HANDLE,
    /// Instance id of the ViGEmBus device node this handle was opened on
    /// (NUL-terminated UTF-16). `None` if it could not be read - serial
    /// discovery then degrades to [`crate::SerialDiscovery::Off`].
    pub(crate) devnode_id: Option<Vec<u16>>,
}

impl BusInner {
    /// Test helper: no captured instance id.
    #[cfg(test)]
    #[inline]
    pub(crate) fn new(handle: HANDLE) -> Self {
        Self {
            handle,
            devnode_id: None,
        }
    }
}

impl Drop for BusInner {
    fn drop(&mut self) {
        // SAFETY: sole closer of this handle.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// SAFETY: Win32 HANDLEs have no thread affinity; concurrent DeviceIoControl is fine.
unsafe impl Send for BusInner {}
// SAFETY: same as Send.
unsafe impl Sync for BusInner {}

/// Storage the I/O manager may write into after submission.
///
/// Heap-allocated and address-stable. On a drain timeout this allocation is
/// deliberately leaked: the kernel may still hold pointers into it, and there
/// is no bounded way to prove otherwise.
#[repr(C)]
struct KernelOwned {
    overlapped: OVERLAPPED,
    transferred: u32,
}

/// One `METHOD_BUFFERED` IOCTL: `OVERLAPPED`, event, and `Arc` to keep the device open.
///
/// - Do not move while `pending` (kernel holds a pointer into the boxed I/O block).
/// - Use raw pointers into Win32; never reborrow the I/O block while an IRP may reference it.
/// - Drop drains with a bounded wait. On timeout the event handle, the boxed I/O
///   block, *and* the `Arc<BusInner>` are intentionally leaked so the device handle
///   cannot be closed under a live IRP; the call is poisoned and never reused.
/// - Callers that pass their own input/output buffers must leak those buffers too
///   when poisoned. Prefer [`IoctlBuffers`], which owns the call + both boxes and
///   enforces finish-then-free-or-leak (including on panic unwind).
/// - `pending` means the kernel may still write this `OVERLAPPED` (failed completion still clears it).
pub(crate) struct OverlappedCall {
    io: ManuallyDrop<Box<KernelOwned>>,
    /// Leaked with `io` when poisoned.
    bus: ManuallyDrop<Arc<BusInner>>,
    pending: bool,
    /// Drain timed out; never close or reuse the event.
    leak_event: bool,
    _pin: PhantomPinned,
}

// SAFETY: HANDLEs have no thread affinity; OVERLAPPED is exclusive while submitted.
unsafe impl Send for OverlappedCall {}

impl OverlappedCall {
    pub(crate) fn new(bus: Arc<BusInner>) -> windows::core::Result<Self> {
        // SAFETY: unnamed auto-reset event; failure becomes Err.
        let h_event = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self {
            io: ManuallyDrop::new(Box::new(KernelOwned {
                overlapped: OVERLAPPED {
                    hEvent: h_event,
                    ..Default::default()
                },
                transferred: 0,
            })),
            bus: ManuallyDrop::new(bus),
            pending: false,
            leak_event: false,
            _pin: PhantomPinned,
        })
    }

    fn handle(&self) -> HANDLE {
        self.bus.handle
    }

    /// `*const OVERLAPPED` for cross-thread `CancelIoEx`.
    pub(crate) fn overlapped_ptr(&self) -> *const OVERLAPPED {
        &raw const self.io.overlapped
    }

    fn overlapped_mut_ptr(&mut self) -> *mut OVERLAPPED {
        &raw mut self.io.overlapped
    }

    fn transferred_mut_ptr(&mut self) -> *mut u32 {
        &raw mut self.io.transferred
    }

    /// `true` if the kernel has written a final status into the I/O block.
    fn irp_completed(&self) -> bool {
        self.io.overlapped.Internal != STATUS_PENDING
    }

    /// Drain timed out; I/O block, event, and caller buffers must stay leaked.
    pub(crate) fn is_poisoned(&self) -> bool {
        self.leak_event
    }

    /// Drain if needed and drop. `true` if poisoned (caller must leak buffers).
    pub(crate) fn finish(mut self) -> bool {
        if !self.is_poisoned() && self.pending {
            let _ = self.drain_bounded();
        }
        let poisoned = self.is_poisoned();
        // Drop frees if drained, leaks if `leak_event`; no second drain.
        poisoned
    }

    fn drain_timeout_error() -> windows::core::Error {
        windows::core::Error::new(
            ERROR_TIMEOUT.to_hresult(),
            "OverlappedCall: cancel/drain timed out; OVERLAPPED still owned by the kernel",
        )
    }

    /// Cancel a pending IRP; wait at most [`DRAIN_TIMEOUT_MS`].
    ///
    /// Do not treat `irp_completed()` alone as drained (`Internal` starts at 0 from `Default`).
    fn drain_bounded(&mut self) -> DrainOutcome {
        if self.leak_event {
            return DrainOutcome::TimedOut;
        }
        if !self.pending {
            return DrainOutcome::Drained;
        }

        // SAFETY: `bus` keeps the device open; `io.overlapped` is the pending OVERLAPPED.
        let cancel = unsafe { CancelIoEx(self.handle(), Some(self.overlapped_ptr())) };

        // NOT_FOUND + completed status: IRP already done (do not wait on the event).
        if let Err(e) = &cancel
            && e.code() == ERROR_NOT_FOUND.to_hresult()
            && self.irp_completed()
        {
            // SAFETY: IRP finished; harvest without waiting.
            unsafe {
                let _ = GetOverlappedResult(
                    self.handle(),
                    self.overlapped_mut_ptr(),
                    self.transferred_mut_ptr(),
                    false,
                );
            }
            self.pending = false;
            return DrainOutcome::Drained;
        }

        // SAFETY: event owned by this call and still open.
        let wait = unsafe { WaitForSingleObject(self.io.overlapped.hEvent, DRAIN_TIMEOUT_MS) };
        if wait == WAIT_OBJECT_0 {
            // SAFETY: same OVERLAPPED; handle still live.
            unsafe {
                let _ = GetOverlappedResult(
                    self.handle(),
                    self.overlapped_mut_ptr(),
                    self.transferred_mut_ptr(),
                    false,
                );
            }
            self.pending = false;
            DrainOutcome::Drained
        } else {
            self.leak_event = true;
            DrainOutcome::TimedOut
        }
    }

    /// Prepare for another submit (same event). Drains any leftover pending IRP.
    ///
    /// # Errors
    /// Drain timeout: call is poisoned (`leak_event`); do not reuse.
    pub(crate) fn reset(&mut self) -> Result<(), windows::core::Error> {
        if self.leak_event {
            return Err(Self::drain_timeout_error());
        }

        if self.pending {
            match self.drain_bounded() {
                DrainOutcome::Drained => {}
                DrainOutcome::TimedOut => {
                    return Err(Self::drain_timeout_error());
                }
            }
        }

        debug_assert!(!self.pending);
        debug_assert!(!self.io.overlapped.hEvent.is_invalid());

        let h_event = self.io.overlapped.hEvent;
        // SAFETY: event owned by this call; ResetEvent is valid on an open event handle.
        unsafe {
            let _ = ResetEvent(h_event);
        }
        self.io.overlapped = OVERLAPPED {
            hEvent: h_event,
            ..Default::default()
        };
        self.io.transferred = 0;
        Ok(())
    }

    /// Blocking `METHOD_BUFFERED` IOCTL on a reusable call (resets first).
    ///
    /// Waits at most `timeout_ms` for a pending IRP. On timeout the call is left
    /// `pending` (not poisoned yet); a later `reset` / `finish` / Drop drains
    /// (and may poison).
    ///
    /// # Safety
    /// - `input` valid for `in_len` (or null/0) for the whole call
    /// - `output` valid for `out_len` (or null/0) through IRP completion
    /// - `output` must not alias `input` when both non-null
    /// - do not move while an IRP is pending
    pub(crate) unsafe fn ioctl_timeout(
        &mut self,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
        timeout_ms: u32,
    ) -> Result<u32, windows::core::Error> {
        self.reset()?;

        let handle = self.handle();
        debug_assert!(!self.io.overlapped.hEvent.is_invalid());

        let in_buf = if input.is_null() { None } else { Some(input) };
        let out_buf = if output.is_null() { None } else { Some(output) };

        // One raw OVERLAPPED pointer for submit and wait - no re-borrow of the I/O block.
        let overlapped = self.overlapped_mut_ptr();
        let transferred = self.transferred_mut_ptr();

        // SAFETY: caller guarantees buffers; OVERLAPPED/event owned by `self`.
        let result = unsafe {
            DeviceIoControl(
                handle,
                code,
                in_buf,
                in_len,
                out_buf,
                out_len,
                Some(transferred),
                Some(overlapped),
            )
        };

        let n = match result {
            Ok(()) => {
                // SAFETY: `transferred` is `self.io.transferred`.
                unsafe { *transferred }
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                self.pending = true;
                self.wait_bounded(timeout_ms)?
            }
            Err(e) => return Err(e),
        };

        ensure_full_transfer(n, out_len)?;
        Ok(n)
    }

    /// [`ioctl_timeout`](Self::ioctl_timeout) with [`IOCTL_DEFAULT_TIMEOUT_MS`].
    ///
    /// # Safety
    /// Same as [`ioctl_timeout`](Self::ioctl_timeout).
    pub(crate) unsafe fn ioctl(
        &mut self,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
    ) -> Result<u32, windows::core::Error> {
        // SAFETY: forwarded unchanged; caller upholds the same contract.
        unsafe {
            self.ioctl_timeout(
                code,
                input,
                in_len,
                output,
                out_len,
                IOCTL_DEFAULT_TIMEOUT_MS,
            )
        }
    }

    /// One-shot blocking IOCTL with an explicit wait bound (allocates event, drops).
    ///
    /// Null-output only: Drop may poison without telling the caller, so heap
    /// output buffers cannot be freed safely. Use a local [`OverlappedCall`] +
    /// [`finish`](Self::finish) (and leak buffers on poison) when `out_len > 0`.
    ///
    /// # Safety
    /// Same buffer requirements as [`ioctl_timeout`](Self::ioctl_timeout). Prefer
    /// null `output` unless the caller can observe poison another way.
    pub(crate) unsafe fn ioctl_sync_timeout(
        bus: &Arc<BusInner>,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
        timeout_ms: u32,
    ) -> Result<u32, windows::core::Error> {
        let mut call = Self::new(Arc::clone(bus))?;
        // SAFETY: caller guarantees buffers; `call` is not moved while pending.
        unsafe { call.ioctl_timeout(code, input, in_len, output, out_len, timeout_ms) }
    }

    /// [`ioctl_sync_timeout`](Self::ioctl_sync_timeout) with [`IOCTL_DEFAULT_TIMEOUT_MS`].
    ///
    /// # Safety
    /// Same as [`ioctl_sync_timeout`](Self::ioctl_sync_timeout).
    pub(crate) unsafe fn ioctl_sync(
        bus: &Arc<BusInner>,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
    ) -> Result<u32, windows::core::Error> {
        // SAFETY: forwarded unchanged; caller upholds the same contract.
        unsafe {
            Self::ioctl_sync_timeout(
                bus,
                code,
                input,
                in_len,
                output,
                out_len,
                IOCTL_DEFAULT_TIMEOUT_MS,
            )
        }
    }

    /// Submit without waiting (notification workers). Resets first, like
    /// [`ioctl`](Self::ioctl).
    ///
    /// `pending` only on `ERROR_IO_PENDING`. Drain timeout poisons the call
    /// (`ERROR_TIMEOUT`); do not reuse.
    ///
    /// # Safety
    /// Same buffer requirements as [`ioctl`](Self::ioctl).
    pub(crate) unsafe fn submit(
        &mut self,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
    ) -> Result<(), windows::core::Error> {
        self.reset()?;

        debug_assert!(!self.io.overlapped.hEvent.is_invalid());

        let in_buf = if input.is_null() { None } else { Some(input) };
        let out_buf = if output.is_null() { None } else { Some(output) };
        let handle = self.handle();
        let overlapped = self.overlapped_mut_ptr();
        let transferred = self.transferred_mut_ptr();

        // SAFETY: caller guarantees buffers; OVERLAPPED is address-stable in the heap box.
        let result = unsafe {
            DeviceIoControl(
                handle,
                code,
                in_buf,
                in_len,
                out_buf,
                out_len,
                Some(transferred),
                Some(overlapped),
            )
        };

        match result {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                self.pending = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Harvest completion (`GetOverlappedResult`). Clears `pending` if the IRP finished.
    ///
    /// Callers that wait for the event should use [`wait_bounded`](Self::wait_bounded)
    /// or [`wait_cancellable`](Self::wait_cancellable) first and pass `wait = false`
    /// here - never block unboundedly inside this helper.
    fn take_overlapped_result(&mut self, wait: bool) -> Result<u32, windows::core::Error> {
        let handle = self.handle();
        let overlapped = self.overlapped_mut_ptr();
        let transferred = self.transferred_mut_ptr();
        // SAFETY: submitted against this handle; event live; raw pointers only.
        let result = unsafe { GetOverlappedResult(handle, overlapped, transferred, wait) };
        if result.is_err() && self.irp_completed() {
            self.pending = false;
        }
        result?;
        self.pending = false;
        Ok(self.io.transferred)
    }

    /// Wait at most `timeout_ms` for the pending IRP.
    ///
    /// On timeout, `pending` stays set and the event is *not* consumed, so a later
    /// `reset` / `finish` / Drop can still cancel and drain. Returns `ERROR_TIMEOUT`.
    fn wait_bounded(&mut self, timeout_ms: u32) -> Result<u32, windows::core::Error> {
        if !self.pending {
            return Ok(self.io.transferred);
        }
        // Completed already (including failed IRPs); event may already have been consumed.
        if self.irp_completed() {
            return self.take_overlapped_result(false);
        }
        // SAFETY: event owned by this call and still open while not poisoned.
        let wait = unsafe { WaitForSingleObject(self.io.overlapped.hEvent, timeout_ms) };
        if wait == WAIT_OBJECT_0 {
            return self.take_overlapped_result(false);
        }
        if wait == WAIT_TIMEOUT {
            return Err(windows::core::Error::new(
                ERROR_TIMEOUT.to_hresult(),
                "OverlappedCall: IOCTL wait timed out; IRP still pending",
            ));
        }
        Err(windows::core::Error::new(
            ERROR_TIMEOUT.to_hresult(),
            "OverlappedCall: WaitForSingleObject failed while waiting for IRP",
        ))
    }

    /// [`wait_bounded`](Self::wait_bounded) with [`IOCTL_DEFAULT_TIMEOUT_MS`].
    /// Notification workers use [`wait_cancellable`](Self::wait_cancellable) instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn wait(&mut self) -> Result<u32, windows::core::Error> {
        self.wait_bounded(IOCTL_DEFAULT_TIMEOUT_MS)
    }

    /// Wait for a pending IRP, re-checking `should_abort` every `poll_ms`.
    ///
    /// Notification workers use this so Drop's shutdown flag is observed even when
    /// `CancelIoEx` does not complete the IRP. Abort while still pending returns
    /// `ERROR_OPERATION_ABORTED` and leaves `pending` for a later
    /// [`finish`](Self::finish) / Drop drain.
    pub(crate) fn wait_cancellable(
        &mut self,
        poll_ms: u32,
        mut should_abort: impl FnMut() -> bool,
    ) -> Result<u32, windows::core::Error> {
        if !self.pending {
            return Ok(self.io.transferred);
        }

        loop {
            if should_abort() {
                return match self.take_overlapped_result(false) {
                    Ok(n) => Ok(n),
                    Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => {
                        Err(windows::core::Error::from(ERROR_OPERATION_ABORTED))
                    }
                    Err(e) => Err(e),
                };
            }

            // SAFETY: event owned by this call and still open while not poisoned.
            let wait = unsafe { WaitForSingleObject(self.io.overlapped.hEvent, poll_ms) };
            if wait == WAIT_OBJECT_0 {
                return self.take_overlapped_result(false);
            }
            if wait == WAIT_TIMEOUT {
                continue;
            }
            // WAIT_FAILED (or unexpected status): surface as a generic wait failure.
            return Err(windows::core::Error::new(
                ERROR_TIMEOUT.to_hresult(),
                "OverlappedCall: WaitForSingleObject failed while waiting for IRP",
            ));
        }
    }
}

pub(crate) fn ensure_full_transfer(
    transferred: u32,
    out_len: u32,
) -> Result<(), windows::core::Error> {
    if out_len > 0 && transferred < out_len {
        return Err(windows::core::Error::new(
            ERROR_INSUFFICIENT_BUFFER.to_hresult(),
            "IOCTL completed with fewer bytes than the output buffer size",
        ));
    }
    Ok(())
}

impl Drop for OverlappedCall {
    fn drop(&mut self) {
        if self.leak_event {
            // Prior drain timed out: `io`, event, and `bus` Arc stay leaked.
            return;
        }

        if self.pending {
            match self.drain_bounded() {
                DrainOutcome::Drained => {}
                DrainOutcome::TimedOut => return,
            }
        }

        // SAFETY: event from `new`; closed exactly once. The IRP is drained, so
        // the kernel holds no pointer into `io` and no device reference from this
        // call remains - safe to drop the bus Arc (may CloseHandle the device).
        unsafe {
            let _ = CloseHandle(self.io.overlapped.hEvent);
            ManuallyDrop::drop(&mut self.io);
            ManuallyDrop::drop(&mut self.bus);
        }
    }
}

/// Heap in/out wire buffers owned together with an [`OverlappedCall`].
///
/// Until the IRP completes, `METHOD_BUFFERED` may still write the output buffer,
/// and the input must stay allocated. This type:
///
/// 1. Always finishes the call **before** freeing the boxes (including panic unwind).
/// 2. **Leaks** both boxes when the call is poisoned (drain timeout), matching the
///    protocol documented on [`OverlappedCall`].
///
/// Teardown order is structural, so local declaration order does not matter.
pub(crate) struct IoctlBuffers<T> {
    request: ManuallyDrop<Box<T>>,
    response: ManuallyDrop<Box<T>>,
    call: ManuallyDrop<OverlappedCall>,
    call_done: bool,
    /// Kernel may still reference the boxes; do not free them.
    leak_buffers: bool,
}

impl<T> IoctlBuffers<T> {
    pub(crate) fn new(
        bus: Arc<BusInner>,
        request: Box<T>,
        response: Box<T>,
    ) -> windows::core::Result<Self> {
        let call = OverlappedCall::new(bus)?;
        Ok(Self {
            request: ManuallyDrop::new(request),
            response: ManuallyDrop::new(response),
            call: ManuallyDrop::new(call),
            call_done: false,
            leak_buffers: false,
        })
    }

    /// The overlapped call (must not be used after [`Self::finish`]).
    #[inline]
    pub(crate) fn call(&mut self) -> &mut OverlappedCall {
        debug_assert!(
            !self.call_done,
            "IoctlBuffers::call after finish/drop of the call"
        );
        &mut self.call
    }

    #[inline]
    pub(crate) fn response(&self) -> &T {
        &self.response
    }

    #[inline]
    pub(crate) fn response_mut(&mut self) -> &mut T {
        &mut self.response
    }

    /// Stable `*const` into the request box for `DeviceIoControl`.
    #[inline]
    pub(crate) fn request_as_void(&self) -> *const c_void {
        (&**self.request as *const T).cast::<c_void>()
    }

    /// Stable `*mut` into the response box for `DeviceIoControl`.
    #[inline]
    pub(crate) fn response_as_void_mut(&mut self) -> *mut c_void {
        (&mut **self.response as *mut T).cast::<c_void>()
    }

    /// Drain the call. `true` if poisoned (buffers leaked on drop). Idempotent.
    pub(crate) fn finish(&mut self) -> bool {
        if self.call_done {
            return self.leak_buffers;
        }
        self.call_done = true;
        // SAFETY: `call_done` was false; `call` is still initialized.
        let call = unsafe { ManuallyDrop::take(&mut self.call) };
        let poisoned = call.finish();
        self.leak_buffers = poisoned;
        poisoned
    }
}

impl<T> Drop for IoctlBuffers<T> {
    fn drop(&mut self) {
        // Finish before freeing boxes (covers unwind that skipped [`Self::finish`]).
        if !self.call_done {
            // SAFETY: `call` still initialized.
            let call = unsafe { ManuallyDrop::take(&mut self.call) };
            self.call_done = true;
            if call.finish() {
                self.leak_buffers = true;
            }
        }

        if self.leak_buffers {
            // Intentional leak: kernel may still write into these allocations.
            return;
        }

        // SAFETY: not poisoned; IRP no longer references these boxes.
        unsafe {
            ManuallyDrop::drop(&mut self.request);
            ManuallyDrop::drop(&mut self.response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, GetHandleInformation};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::core::w;

    /// Overlapped `NUL` handle for `CancelIoEx` tests without ViGEmBus.
    fn open_nul_overlapped() -> HANDLE {
        // SAFETY: open the well-known NUL device; caller owns the handle.
        unsafe {
            CreateFileW(
                w!("NUL"),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .expect("CreateFileW(NUL) with FILE_FLAG_OVERLAPPED")
    }

    #[test]
    fn ioctl_buffers_finish_and_drop_without_pending_free_boxes() {
        let bus = Arc::new(BusInner::new(HANDLE::default()));
        let mut io = IoctlBuffers::new(Arc::clone(&bus), Box::new(1u32), Box::new(2u32))
            .expect("CreateEventW");
        assert_eq!(*io.response(), 2);
        assert!(!io.request_as_void().is_null());
        assert!(!io.finish(), "non-pending finish must not poison");
        assert!(!io.finish());
        drop(io);

        // Drop path without explicit finish (unwind-equivalent clean drop).
        let io = IoctlBuffers::new(Arc::clone(&bus), Box::new(3u32), Box::new(4u32))
            .expect("CreateEventW");
        drop(io);

        let call = OverlappedCall::new(bus).expect("CreateEventW");
        assert!(!call.finish(), "idle call must finish clean");
    }

    #[test]
    fn reset_preserves_event_handle_and_clears_state() {
        let bus = Arc::new(BusInner::new(HANDLE::default()));
        let mut call = OverlappedCall::new(bus).expect("CreateEventW should succeed");
        let event = call.io.overlapped.hEvent;
        call.io.transferred = 42;
        call.reset().expect("reset with no pending IRP");
        assert_eq!(
            call.io.overlapped.hEvent, event,
            "reset must reuse the same event"
        );
        assert_eq!(call.io.transferred, 0);
        assert!(!call.pending);
        for _ in 0..8 {
            call.reset().expect("reset");
            assert_eq!(call.io.overlapped.hEvent, event);
        }
    }

    #[test]
    fn irp_completed_tracks_status_pending_in_internal() {
        let bus = Arc::new(BusInner::new(HANDLE::default()));
        let mut call = OverlappedCall::new(bus).expect("CreateEventW");
        // Fresh OVERLAPPED has Internal == 0 from Default - already "completed"
        // from the HasOverlappedIoCompleted perspective (not STATUS_PENDING).
        assert!(call.irp_completed());
        call.io.overlapped.Internal = STATUS_PENDING;
        assert!(!call.irp_completed());
        // Any final NTSTATUS (success or failure) counts as completed.
        call.io.overlapped.Internal = 0; // STATUS_SUCCESS
        assert!(call.irp_completed());
        call.io.overlapped.Internal = 0xC000_0001; // STATUS_UNSUCCESSFUL
        assert!(call.irp_completed());
    }

    /// Abort while IRP is still incomplete leaves `pending` for a later drain.
    #[test]
    fn wait_cancellable_abort_leaves_pending_when_incomplete() {
        let handle = open_nul_overlapped();
        let bus = Arc::new(BusInner::new(handle));
        let mut call = OverlappedCall::new(bus).expect("CreateEventW");
        call.pending = true;
        call.io.overlapped.Internal = STATUS_PENDING;

        let err = call
            .wait_cancellable(50, || true)
            .expect_err("abort must return before an incomplete IRP finishes");
        assert_eq!(err.code(), ERROR_OPERATION_ABORTED.to_hresult());
        assert!(
            call.pending,
            "pending must stay set so finish/Drop can drain or poison"
        );

        // No real IRP; clear for clean Drop.
        call.pending = false;
    }

    /// Failed completion clears `pending` so `reset` can reuse the call.
    #[test]
    fn pending_cleared_when_irp_completed_with_failure() {
        let handle = open_nul_overlapped();
        let bus = Arc::new(BusInner::new(handle));
        let mut call = OverlappedCall::new(bus).expect("CreateEventW");

        call.pending = true;
        // Kernel-written failure status; event may already have been consumed.
        call.io.overlapped.Internal = 0xC000_0001; // STATUS_UNSUCCESSFUL

        let err = call
            .wait()
            .expect_err("GetOverlappedResult should surface the IRP failure");
        let _ = err;
        assert!(
            !call.pending,
            "pending must clear when Internal is no longer STATUS_PENDING"
        );
        assert!(
            !call.leak_event,
            "completed failure must not arm the drain-timeout leak path"
        );

        call.reset()
            .expect("call must be reusable after a completed-but-failed wait");
        assert!(!call.pending);
        assert!(!call.leak_event);
    }

    /// `CancelIoEx` `NOT_FOUND` + completed IRP drains immediately.
    #[test]
    fn drain_not_found_with_completed_irp_returns_immediately() {
        let handle = open_nul_overlapped();
        let bus = Arc::new(BusInner::new(handle));
        let mut call = OverlappedCall::new(bus).expect("CreateEventW");

        // Stale pending + completed Internal, no IRP actually queued.
        // CancelIoEx on a live handle with no matching request -> ERROR_NOT_FOUND.
        call.pending = true;
        assert!(
            call.irp_completed(),
            "Default Internal is not STATUS_PENDING"
        );

        let start = Instant::now();
        assert_eq!(call.drain_bounded(), DrainOutcome::Drained);
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "ERROR_NOT_FOUND + completed IRP must not wait the full drain timeout; took {elapsed:?}"
        );
        assert!(!call.pending);
        assert!(!call.leak_event);
        call.reset()
            .expect("reset must succeed after NOT_FOUND drain");
    }

    /// Timed-out drain poisons the call across reset / `is_poisoned` / finish / drop.
    ///
    /// Setup: pending IRP on a dead handle so `CancelIoEx` cannot complete the event.
    /// Each entry point is exercised with its own call (drain runs once per call).
    #[test]
    fn timed_out_drain_poisons_all_entry_points() {
        // --- reset: drain timeout, refuse reuse, stay poisoned ---
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(bus).expect("CreateEventW");
            call.pending = true;
            let event = call.io.overlapped.hEvent;

            let err = call
                .reset()
                .expect_err("reset must fail when drain times out");
            assert_eq!(err.code(), ERROR_TIMEOUT.to_hresult());
            assert!(call.leak_event);
            assert!(call.is_poisoned());
            assert!(call.pending);

            let err2 = call.reset().expect_err("second reset still poisoned");
            assert_eq!(err2.code(), ERROR_TIMEOUT.to_hresult());

            drop(call);
            // SAFETY: Drop skipped CloseHandle (leak_event).
            unsafe {
                let mut flags = 0u32;
                GetHandleInformation(event, &mut flags).expect("event still open after leak drop");
                let _ = CloseHandle(event);
            }
        }

        // --- finish: reports poison when cancel/drain times out ---
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(bus).expect("CreateEventW");
            call.pending = true;
            let event = call.io.overlapped.hEvent;

            assert!(
                call.finish(),
                "finish must report poison when cancel/drain times out"
            );

            // SAFETY: finish/Drop leaked the event on the poison path.
            unsafe {
                let mut flags = 0u32;
                GetHandleInformation(event, &mut flags)
                    .expect("event still open after finish poison");
                let _ = CloseHandle(event);
            }
        }

        // --- drop: returns within ~DRAIN_TIMEOUT_MS and leaks the event ---
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(bus).expect("CreateEventW should succeed");
            call.pending = true;
            let event = call.io.overlapped.hEvent;

            let start = Instant::now();
            drop(call);
            let elapsed = start.elapsed();

            assert!(
                elapsed < Duration::from_millis(u64::from(DRAIN_TIMEOUT_MS) * 2 + 500),
                "drop took {elapsed:?}; expected ~{DRAIN_TIMEOUT_MS}ms bounded drain"
            );
            assert!(
                elapsed >= Duration::from_millis(u64::from(DRAIN_TIMEOUT_MS).saturating_sub(200)),
                "drop returned in {elapsed:?}; expected to wait roughly the drain timeout \
                 (CancelIoEx on invalid handle should not complete the event early)"
            );

            // SAFETY: `event` was created by CreateEventW and Drop skipped CloseHandle.
            unsafe {
                let mut flags = 0u32;
                GetHandleInformation(event, &mut flags)
                    .expect("timed-out drain must leak the event (still a valid handle)");
                let _ = CloseHandle(event);
            }
        }
    }

    /// Clean drop releases `Arc<BusInner>`; poisoned drop leaks it so the device
    /// handle cannot be closed under a live IRP.
    #[test]
    fn arc_release_vs_deliberate_leak() {
        // Clean path.
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let call = OverlappedCall::new(Arc::clone(&bus)).expect("CreateEventW");
            assert_eq!(Arc::strong_count(&bus), 2);
            drop(call);
            assert_eq!(Arc::strong_count(&bus), 1);
        }

        // Poison path: pending IRP on a dead handle times out the drain.
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(Arc::clone(&bus)).expect("CreateEventW");
            call.pending = true;
            let event = call.io.overlapped.hEvent;
            drop(call);
            assert_eq!(
                Arc::strong_count(&bus),
                2,
                "poisoned call must leak its Arc<BusInner> so CloseHandle cannot run under a live IRP"
            );
            // SAFETY: poisoned Drop skipped CloseHandle; reclaim so the test process does not leak.
            unsafe {
                let _ = CloseHandle(event);
            }
        }
    }

    /// `wait_bounded` returns `ERROR_TIMEOUT` without hanging when the IRP never
    /// completes; non-pending is an immediate no-op.
    #[test]
    fn ioctl_wait_is_bounded_when_irp_never_completes() {
        // Non-pending: no wait.
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(bus).expect("CreateEventW");
            call.io.transferred = 7;
            assert!(!call.pending);

            let start = Instant::now();
            let n = call
                .wait_bounded(5_000)
                .expect("non-pending wait_bounded is a no-op");
            let elapsed = start.elapsed();

            assert_eq!(n, 7);
            assert!(
                elapsed < Duration::from_millis(100),
                "non-pending wait_bounded must return immediately; took {elapsed:?}"
            );
        }

        // Pending + never completes: bounded timeout, no poison.
        {
            let bus = Arc::new(BusInner::new(HANDLE::default()));
            let mut call = OverlappedCall::new(bus).expect("CreateEventW");
            call.pending = true;
            call.io.overlapped.Internal = STATUS_PENDING;

            let start = Instant::now();
            let err = call
                .wait_bounded(200)
                .expect_err("dead handle + STATUS_PENDING must time out");
            let elapsed = start.elapsed();

            assert_eq!(err.code(), ERROR_TIMEOUT.to_hresult());
            assert!(
                elapsed < Duration::from_millis(2_000),
                "wait_bounded(200) took {elapsed:?}; expected well under 2 s"
            );
            assert!(
                elapsed >= Duration::from_millis(150),
                "wait_bounded(200) returned in {elapsed:?}; expected ~200 ms"
            );
            assert!(
                call.pending,
                "timeout must leave pending so reset/finish/Drop can drain"
            );
            assert!(
                !call.leak_event,
                "wait_bounded timeout must not poison; only drain does"
            );

            // Clear pending so Drop does not spend DRAIN_TIMEOUT_MS.
            call.pending = false;
        }
    }
}
