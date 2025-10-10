use anyhow::Result;
use rubato::{FftFixedIn, VecResampler};

pub struct Resampler {
    ratio: f64,
    fft: FftFixedIn<f64>,
    src_sample_rate: u32,
    dst_sample_rate: u32,
    src_pcm_len: usize,
}

impl Resampler {
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

    pub fn convert(&mut self, src: &[i16]) -> Vec<i16> {
        let src_len = src.len();

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
