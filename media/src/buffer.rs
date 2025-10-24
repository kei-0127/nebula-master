//! # Media Buffer Management
//! 
//! Audio/video buffer management for real-time media processing.
//! Provides jitter buffers, sequence-ordered queues, and audio processing buffers.

use crate::{packet::RtpPacket, server::MEDIA_SERVICE};
use anyhow::Result;
use codec::Codec;
use nnnoiseless::DenoiseState;
use parking_lot::Mutex;
use sdp::payload::Rtpmap;
use std::{collections::VecDeque, sync::Arc};
use tracing::warn;

use crate::resampler::Resampler;

// Buffer configuration constants
const MAX_ROCDISORDER: u16 = 100;        // Max out-of-order packets
const MAX_SEQUENCE_NUMBER: u16 = 65535;  // Max RTP sequence number
const MIN_JITTER_BUFFER_SIZE: usize = 2;           // Min packets in buffer
const ABUNDANT_JITTER_BUFFER_SIZE: usize = 2;      // Abundant buffer size
const ABUNDANT_JITTER_BUFFER_TIMES: usize = 500;   // Times to maintain abundant buffer
const MAX_JITTER_BUFFER_SIZE: usize = 10;          // Max packets in buffer

/// Sequence-ordered queue for RTP packets
/// Maintains packets in sequence order, handling out-of-order delivery
pub struct SeqQueue<I> {
    queue: VecDeque<(I, usize)>,  // (item, sequence_number) pairs
}

impl<I> Default for SeqQueue<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I> SeqQueue<I> {
    pub fn new() -> Self {
        SeqQueue {
            queue: VecDeque::new(),
        }
    }

    /// length of items in the queue
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Insert item into queue maintaining sequence order
    /// Highest sequence numbers go to front of queue
    pub fn push(&mut self, item: I, seq: usize) {
        let mut index = None;
        // Find insertion point based on sequence number
        for (i, (_, s)) in self.queue.iter().enumerate() {
            if seq > *s {
                index = Some(i);
                break;
            }
        }

        if let Some(i) = index {
            self.queue.insert(i, (item, seq));
        } else {
            self.queue.push_back((item, seq));
        }
    }

    /// Remove and return the oldest item (lowest sequence number)
    pub fn pop(&mut self) -> Option<(I, usize)> {
        self.queue.pop_back()
    }
}

/// Fixed-size circular buffer for audio data
/// Reuses pre-allocated buffers to minimize memory allocation overhead
pub struct FixedDqueue {
    start: usize,                    // Starting index in circular buffer
    n: Option<usize>,                // Current position index
    size: usize,                     // Total buffer size
    elements: Vec<Vec<u8>>,           // Pre-allocated audio buffers
}

impl FixedDqueue {
    pub fn new(size: usize) -> Self {
        let mut elements = Vec::new();
        for _ in 0..size {
            elements.push(Vec::with_capacity(1500));
        }
        Self {
            size,
            elements,
            n: None,
            start: 0,
        }
    }

    pub fn push(&mut self, element: &[u8]) {
        let n = match self.n {
            Some(n) => n,
            None => self.start,
        };
        if element.len() == self.elements[n].len() {
            self.elements[n].copy_from_slice(element);
        } else {
            self.elements[n].clear();
            self.elements[n].extend_from_slice(element);
        }

        if self.n == Some(self.start) {
            self.start = self.next_index(self.start);
            self.n = Some(self.next_index(n));
        } else {
            self.n = Some(self.next_index(n));
        }
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let n = self.n?;

        let element = self.elements[self.start].clone();
        self.start = self.next_index(self.start);
        if self.start == n {
            self.n = None;
        }
        Some(element)
    }

    fn next_index(&self, i: usize) -> usize {
        if i >= self.size - 1 {
            0
        } else {
            i + 1
        }
    }

    pub fn distance(&self) -> usize {
        let n = match self.n {
            Some(n) => n,
            None => return 0,
        };
        if n <= self.start {
            n + self.size - self.start
        } else {
            n - self.start
        }
    }
}

pub struct RtpDenoiseState {
    denoise: Box<DenoiseState<'static>>,
    denoise_input_buffer: Vec<f32>,
    denoise_output_buffer: Vec<f32>,
}

pub struct RtpBufferState {
    ssrc: u32,
    payloads: SeqQueue<Vec<u8>>,
    pcms: SeqQueue<Vec<i16>>,
    // the number of pcms we cache before we start to get pcms out
    // this is to overcome the jitter of the network
    jitter_buffer_size: usize,
    // We only start to get pcms out if the buffer size was full
    // for the first time
    jitter_buffer_filled: bool,
    // On every time `get` the next src rtp, we check if the buffer is above
    // `ABUNDANT_JITTER_BUFFER_SIZE`, and if it does, we incr this value.
    // when the times is above `ABUNDANT_JITTER_BUFFER_TIMES`, we decrese
    // the jitter_buffer_size
    // this value resets if the buffer is smaller than `ABUNDANT_JITTER_BUFFER_SIZE`
    jitter_buffer_abundant_times: usize,
    codec: Box<dyn Codec>,
    resampler: Resampler,
    last_seq: u16,
    roc: usize,
    roc_has_processed: bool,
    denoise: RtpDenoiseState,
    pcm_len: usize,
    transcode: bool,
    resample: bool,
}

pub struct RtpBuffer {
    state: Arc<Mutex<RtpBufferState>>,
    pub remote_rtpmap: Rtpmap,
}

impl RtpBufferState {
    fn update_roc(&mut self, seq: u16) {
        if !self.roc_has_processed {
            self.roc_has_processed = true;
        } else if seq == 0 {
            if self.last_seq > MAX_ROCDISORDER {
                self.roc += 1;
            }
        } else if self.last_seq < MAX_ROCDISORDER
            && seq > (MAX_SEQUENCE_NUMBER - MAX_ROCDISORDER)
        {
            self.roc -= 1;
        } else if seq < MAX_ROCDISORDER
            && self.last_seq > (MAX_SEQUENCE_NUMBER - MAX_ROCDISORDER)
        {
            self.roc += 1;
        }
        self.last_seq = seq;
    }

    fn denoised_had_sound(&mut self, pcm: &[i16]) -> bool {
        for chunk in pcm.chunks(DenoiseState::FRAME_SIZE) {
            for (i, v) in chunk.iter().enumerate() {
                self.denoise.denoise_input_buffer[i] = *v as f32;
            }
            self.denoise.denoise.process_frame(
                &mut self.denoise.denoise_output_buffer,
                &self.denoise.denoise_input_buffer,
            );
            for (i, _v) in chunk.iter().enumerate() {
                let v = self.denoise.denoise_output_buffer[i] as i16;
                let v = if v >= 0 { v } else { -v } as usize;
                if v > MEDIA_SERVICE.config.silent_threshold {
                    return true;
                }
            }
        }
        false
    }

    fn add(&mut self, seq: u16, payload: &[u8]) -> Result<()> {
        self.update_roc(seq);

        let mut buf = [0; 5000];
        let n = self.codec.decode(payload, &mut buf)?;
        let pcm = if self.resample {
            self.resampler.convert(&buf[..n])
        } else {
            buf[..n].to_vec()
        };

        if pcm.is_empty() {
            return Ok(());
        }

        let seq = seq as usize + MAX_SEQUENCE_NUMBER as usize * self.roc;

        let len = self.pcms.len();
        if (len > 10 || len > self.jitter_buffer_size + 1)
            && !self.denoised_had_sound(&pcm)
        {
            return Ok(());
        }

        if !self.transcode && pcm.len() == self.pcm_len {
            self.payloads.push(payload.to_vec(), seq);
        }
        self.pcms.push(pcm, seq);
        if !self.jitter_buffer_filled && self.pcms.len() >= self.jitter_buffer_size {
            self.jitter_buffer_filled = true;
        }
        Ok(())
    }

    pub fn get(&mut self) -> (Option<Vec<u8>>, Option<Vec<i16>>) {
        if !self.jitter_buffer_filled {
            return (None, None);
        }

        let mut pcm = vec![];
        while let Some((mut p, seq)) = self.pcms.pop() {
            if p.len() + pcm.len() == self.pcm_len {
                if pcm.is_empty() {
                    pcm = p;
                } else {
                    pcm.append(&mut p);
                }
                break;
            }

            if p.len() + pcm.len() > self.pcm_len {
                let remaining = p[self.pcm_len - pcm.len()..].to_vec();
                self.pcms.push(remaining, seq);
                pcm.extend_from_slice(&p[..self.pcm_len - pcm.len()]);
                break;
            }

            pcm.append(&mut p);
        }

        let pcm = if pcm.is_empty() {
            None
        } else if pcm.len() == self.pcm_len {
            Some(pcm)
        } else if pcm.len() < self.pcm_len {
            pcm.append(&mut vec![0i16; self.pcm_len - pcm.len()]);
            Some(pcm)
        } else {
            warn!("retrieve pcm got longer than pcm len");
            Some(pcm)
        };

        let n = self.pcms.len();
        match n {
            _ if n == 0 => {
                // we don't have any packets in the jitter buffer any more
                // mark jitter_buffer_filled to false so that we start re fill the jitter buffer
                // also we increase the buffer size, because the network jitter might be getting worse
                self.jitter_buffer_filled = false;
                if self.jitter_buffer_size < MAX_JITTER_BUFFER_SIZE {
                    self.jitter_buffer_size += 1;
                }
            }
            _ if n >= ABUNDANT_JITTER_BUFFER_SIZE => {
                if self.jitter_buffer_size > MIN_JITTER_BUFFER_SIZE {
                    self.jitter_buffer_abundant_times += 1;
                    if self.jitter_buffer_abundant_times
                        > ABUNDANT_JITTER_BUFFER_TIMES
                    {
                        self.jitter_buffer_size -= 1;
                        self.jitter_buffer_abundant_times = 0;
                    }
                }
            }
            _ => {
                if self.jitter_buffer_abundant_times > 0 {
                    self.jitter_buffer_abundant_times = 0;
                }
            }
        }

        (self.payloads.pop().map(|(i, _o)| i), pcm)
    }
}

impl RtpBuffer {
    pub async fn new(
        ssrc: u32,
        remote_rtpmap: Rtpmap,
        local_rtpmap: Rtpmap,
        local_ptime: usize,
    ) -> Result<RtpBuffer> {
        let codec = remote_rtpmap.name.get_codec().await?;
        let transcode = remote_rtpmap.name != local_rtpmap.name;
        let resample = remote_rtpmap.get_rate() != local_rtpmap.get_rate();
        let resampler =
            Resampler::new(remote_rtpmap.get_rate(), local_rtpmap.get_rate())
                .await?;
        let pcm_len = local_rtpmap.get_rate() as usize / 1000 * local_ptime;

        Ok(RtpBuffer {
            state: Arc::new(Mutex::new(RtpBufferState {
                ssrc,
                pcms: SeqQueue::new(),
                payloads: SeqQueue::new(),
                codec,
                resampler,
                last_seq: 0,
                roc: 0,
                roc_has_processed: false,
                denoise: RtpDenoiseState {
                    denoise: DenoiseState::new(),
                    denoise_input_buffer: vec![0.0; DenoiseState::FRAME_SIZE],
                    denoise_output_buffer: vec![0.0; DenoiseState::FRAME_SIZE],
                },
                transcode,
                pcm_len,
                resample,
                jitter_buffer_size: MIN_JITTER_BUFFER_SIZE,
                jitter_buffer_filled: false,
                jitter_buffer_abundant_times: 0,
            })),
            remote_rtpmap,
        })
    }

    pub async fn add(&self, rtp_packet: RtpPacket) -> Result<()> {
        let state = self.state.clone();
        nebula_task::spawn_priority_task(move || {
            let _ = state
                .lock()
                .add(rtp_packet.sequence(), rtp_packet.payload());
        })
        .await?;
        Ok(())
    }

    pub async fn pop(&self) -> (Option<Vec<u8>>, Option<Vec<i16>>) {
        let state = self.state.clone();
        nebula_task::spawn_priority_task(
            move || -> (Option<Vec<u8>>, Option<Vec<i16>>) { state.lock().get() },
        )
        .await
        .unwrap_or((None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dqueue() {
        let mut queue = FixedDqueue::new(3);
        queue.push(&[0]);
        assert_eq!(queue.pop(), Some(vec![0]));
        assert_eq!(queue.pop(), None);
        queue.push(&[1]);
        queue.push(&[2]);
        queue.push(&[3]);
        queue.push(&[4]);
        assert_eq!(queue.pop(), Some(vec![2]));
        assert_eq!(queue.pop(), Some(vec![3]));
        assert_eq!(queue.pop(), Some(vec![4]));
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.pop(), None);
        queue.push(&[5]);
        queue.push(&[6]);
        assert_eq!(queue.pop(), Some(vec![5]));
        assert_eq!(queue.pop(), Some(vec![6]));
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn test_seq_queue() {
        let mut queue = SeqQueue::new();
        queue.push([4], 4);
        queue.push([2], 2);
        queue.push([3], 3);
        queue.push([1], 1);
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.pop(), Some(([1], 1)));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pop(), Some(([2], 2)));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop(), Some(([3], 3)));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some(([4], 4)));
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.len(), 0);

        queue.push([1], 1);
        queue.push([2], 2);
        queue.push([3], 3);
        queue.push([4], 4);
        assert_eq!(queue.pop(), Some(([1], 1)));
        assert_eq!(queue.pop(), Some(([2], 2)));
        assert_eq!(queue.pop(), Some(([3], 3)));
        assert_eq!(queue.pop(), Some(([4], 4)));
        assert_eq!(queue.pop(), None);

        queue.push([1], 1);
        queue.push([4], 4);
        queue.push([2], 2);
        queue.push([3], 3);
        assert_eq!(queue.pop(), Some(([1], 1)));
        assert_eq!(queue.pop(), Some(([2], 2)));
        assert_eq!(queue.pop(), Some(([3], 3)));
        assert_eq!(queue.pop(), Some(([4], 4)));
        assert_eq!(queue.pop(), None);

        queue.push([1], 1);
        queue.push([4], 4);
        queue.push([3], 3);
        queue.push([2], 2);
        assert_eq!(queue.pop(), Some(([1], 1)));
        assert_eq!(queue.pop(), Some(([2], 2)));
        assert_eq!(queue.pop(), Some(([3], 3)));
        assert_eq!(queue.pop(), Some(([4], 4)));
        assert_eq!(queue.pop(), None);
    }
}
