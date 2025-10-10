use std::ptr;

use anyhow::Result;
use spandsp_sys as ffi;

use crate::Codec;

unsafe impl Send for PCMU {}

pub struct PCMU {
    state: *mut ffi::g711_state_t,
}

impl PCMU {
    pub fn new() -> Self {
        let state = unsafe { ffi::g711_init(ptr::null_mut(), 1) };
        Self { state }
    }
}

impl Drop for PCMU {
    fn drop(&mut self) {
        unsafe {
            ffi::g711_free(self.state);
        }
    }
}

impl Codec for PCMU {
    fn encode(&mut self, src: &[i16], dst: &mut [u8]) -> Result<usize> {
        let o = unsafe {
            ffi::g711_encode(
                self.state,
                dst.as_mut_ptr(),
                src.as_ptr(),
                src.len() as libc::c_int,
            )
        };
        Ok(o as usize)
    }

    fn decode(&mut self, src: &[u8], dst: &mut [i16]) -> Result<usize> {
        let o = unsafe {
            ffi::g711_decode(
                self.state,
                dst.as_mut_ptr(),
                src.as_ptr(),
                src.len() as libc::c_int,
            )
        };
        Ok(o as usize)
    }
}
