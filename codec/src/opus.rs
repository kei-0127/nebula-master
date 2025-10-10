use anyhow::{anyhow, Result};
use libc::c_int;
use opus_sys as ffi;

use crate::Codec;

unsafe impl Send for Opus {}

const OPUS_SET_MAX_BANDWIDTH_REQUEST: c_int = 4004;
const OPUS_SET_INBAND_FEC: c_int = 4012;
const OPUS_SET_DTX: c_int = 4016;

pub struct Opus {
    encoder: *mut ffi::OpusEncoder,
    decoder: *mut ffi::OpusDecoder,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum Application {
    /// Best for most VoIP/videoconference applications where listening quality
    /// and intelligibility matter most.
    Voip = 2048,
    // /// Best for broadcast/high-fidelity application where the decoded audio
    // /// should be as close as possible to the input.
    // Audio = 2049,
    // /// Only use when lowest-achievable latency is what matters most.
    // LowDelay = 2051,
}

impl Codec for Opus {
    fn encode(&mut self, src: &[i16], dst: &mut [u8]) -> Result<usize> {
        let o = unsafe {
            ffi::opus_encode(
                self.encoder,
                src.as_ptr(),
                src.len() as c_int,
                dst.as_mut_ptr(),
                dst.len() as c_int,
            )
        };
        if o < 0 {
            return Err(anyhow!("decode error"));
        }
        Ok(o as usize)
    }

    fn decode(&mut self, src: &[u8], dst: &mut [i16]) -> Result<usize> {
        let o = unsafe {
            ffi::opus_decode(
                self.decoder,
                src.as_ptr(),
                src.len() as c_int,
                dst.as_mut_ptr(),
                dst.len() as c_int,
                0,
            )
        };
        if o < 0 {
            return Err(anyhow!("decode error"));
        }
        Ok(o as usize)
    }
}

impl Opus {
    pub fn new() -> Self {
        let mut error = 0;
        let encoder = unsafe {
            let ptr = ffi::opus_encoder_create(
                48000,
                1,
                Application::Voip as c_int,
                &mut error,
            );
            ffi::opus_encoder_ctl(ptr, OPUS_SET_INBAND_FEC, 1);
            ffi::opus_encoder_ctl(ptr, OPUS_SET_DTX, 1);
            ffi::opus_encoder_ctl(ptr, OPUS_SET_MAX_BANDWIDTH_REQUEST, 64000);
            ptr
        };
        let decoder = unsafe { ffi::opus_decoder_create(48000, 1, &mut error) };
        Self { encoder, decoder }
    }
}

impl Drop for Opus {
    fn drop(&mut self) {
        unsafe {
            ffi::opus_encoder_destroy(self.encoder);
            ffi::opus_decoder_destroy(self.decoder);
        }
    }
}
