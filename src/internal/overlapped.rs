use windows::Win32::Foundation::HANDLE;
use windows::Win32::{
    Foundation::CloseHandle,
    System::{
        IO::{GetOverlappedResult, OVERLAPPED},
        Threading::CreateEventW,
    },
};

pub struct OverlappedCall {
    inner: OVERLAPPED,
    transferred: u32,
}

impl OverlappedCall {
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let inner = OVERLAPPED {
                hEvent: CreateEventW(None, false, false, None)?,
                ..Default::default()
            };
            Ok(Self {
                inner,
                transferred: 0,
            })
        }
    }

    pub fn as_mut_overlapped(&mut self) -> *mut OVERLAPPED {
        &mut self.inner
    }

    pub fn transferred_ptr(&mut self) -> *mut u32 {
        &mut self.transferred
    }

    pub fn wait(&mut self, handle: HANDLE) -> windows::core::Result<u32> {
        unsafe {
            GetOverlappedResult(handle, &self.inner, &mut self.transferred, true)?;
            Ok(self.transferred)
        }
    }
}

impl Drop for OverlappedCall {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.inner.hEvent);
        }
    }
}
