use rand::Rng;
use std::ptr;

use spandsp_sys as ffi;

unsafe impl Send for Noise {}

pub struct Noise {
    state: *mut ffi::noise_state_t,
}

impl Noise {
    pub fn new() -> Self {
        let mut rng = rand::thread_rng();
        let seed: i32 = rng.gen();
        let state = unsafe {
            ffi::noise_init_dbov(
                ptr::null_mut(),
                seed,
                -80f32,
                ffi::NOISE_CLASS_AWGN as std::os::raw::c_int,
                10,
            )
        };
        Self { state }
    }

    pub fn generate(&mut self) -> i16 {
        unsafe { ffi::noise(self.state) }
    }
}
