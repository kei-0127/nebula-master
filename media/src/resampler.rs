//! # Audio Resampler
//! 
//! Audio sample rate conversion using high-quality FFT-based resampling.
//! Converts audio between different sample rates while maintaining quality.

use anyhow::Result;
use rubato::{FftFixedIn, VecResampler};

/// FFT-based mono audio resampler with low-latency, high-quality conversion.
pub struct Resampler {
    ratio: f64,                    // Conversion ratio (dst/src)
    fft: FftFixedIn<f64>,         // FFT-based resampler
    src_sample_rate: u32,          // Input sample rate
    dst_sample_rate: u32,          // Output sample rate
    /// Expected input frame length in samples (mono). If the incoming frame
    /// length changes at runtime, the resampler is reinitialized to match.
    src_pcm_len: usize,            // Input buffer length
}

impl Resampler {
    /// Create an FFT-based mono resampler from `src_sample_rate` to `dst_sample_rate`.
    /// Starts with a default block size and adapts later if the input frame length changes.
    pub async fn new(
        src_sample_rate: u32,
        dst_sample_rate: u32,
    ) -> Result<Resampler> {
        let (ratio, src_pcm_len, fft) = nebula_task::spawn_task(move || {
            let ratio = dst_sample_rate as f64 / src_sample_rate as f64;
            let src_pcm_len = 160;
            let fft = FftFixedIn::new(
                src_sample_rate as usize,
                dst_sample_rate as usize,
                src_pcm_len,
                1,
                1,
            );
            (ratio, src_pcm_len, fft)
        })
        .await?;

        Ok(Resampler {
            ratio,
            fft,
            src_sample_rate,
            dst_sample_rate,
            src_pcm_len,
        })
    }

    
    /// Resample one mono PCM frame to the target rate (approx length = `src.len() * ratio`).
    /// If the frame size changes, reinitialize; on internal errors, return a zeroed frame.
    pub fn convert(&mut self, src: &[i16]) -> Vec<i16> {
        let src_len = src.len();

        // Reconfigure the FFT resampler if the input frame size changed.
        if src_len != self.src_pcm_len {
            self.src_pcm_len = src_len;
            self.fft = FftFixedIn::new(
                self.src_sample_rate as usize,
                self.dst_sample_rate as usize,
                self.src_pcm_len,
                1,
                1,
            );
        }

        // Pre-compute a fallback output length for error cases.
        let dst_len = (src.len() as f64 * self.ratio) as usize;

        let src: Vec<f64> = src.iter().map(|i| *i as f64).collect();

        match self.fft.process(&[src]) {
            Ok(outputs) => outputs
                .get(0)
                .map(|output| {
                    output
                        .iter()
                        .map(|amp| amp.round() as i16)
                        .collect::<Vec<i16>>()
                })
                .unwrap_or_else(|| vec![0; dst_len]),
            Err(_) => vec![0; dst_len],
        }
    }
}
