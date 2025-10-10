use anyhow::Result;
use libc;
use spandsp_sys as ffi;
use std::ptr;

use crate::Codec;

unsafe impl Send for G722Codec {}

pub struct G722Codec {
    encode_state: *mut ffi::g722_encode_state_t,
    decode_state: *mut ffi::g722_decode_state_t,
}

impl Codec for G722Codec {
    fn encode(&mut self, src: &[i16], dst: &mut [u8]) -> Result<usize> {
        let o = unsafe {
            ffi::g722_encode(
                self.encode_state,
                dst.as_mut_ptr(),
                src.as_ptr(),
                src.len() as libc::c_int,
            )
        };
        Ok(o as usize)
    }

    fn decode(&mut self, src: &[u8], dst: &mut [i16]) -> Result<usize> {
        let o = unsafe {
            ffi::g722_decode(
                self.decode_state,
                dst.as_mut_ptr(),
                src.as_ptr(),
                src.len() as libc::c_int,
            )
        };
        Ok(o as usize)
    }
}

impl G722Codec {
    pub fn new() -> Self {
        let encode_state = unsafe {
            ffi::g722_encode_init(
                ptr::null_mut(),
                ffi::G722_SAMPLE_RATE_8000 as i32,
                0,
            )
        };
        let decode_state = unsafe {
            ffi::g722_decode_init(
                ptr::null_mut(),
                ffi::G722_SAMPLE_RATE_8000 as i32,
                0,
            )
        };
        G722Codec {
            encode_state,
            decode_state,
        }
    }
}

impl Drop for G722Codec {
    fn drop(&mut self) {
        unsafe {
            ffi::g722_encode_free(self.encode_state);
            ffi::g722_decode_free(self.decode_state);
        }
    }
}
