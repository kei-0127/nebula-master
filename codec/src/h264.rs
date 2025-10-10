use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder};
use ffmpeg_next::{
    codec::{Context, Id},
    decoder,
    ffi::AVPictureType,
    Packet,
};
use itertools::Itertools;

use crate::VideoCodec;

enum H264Encoder {
    Opened(ffmpeg_next::encoder::Video),
    Unopened(ffmpeg_next::encoder::video::Video),
}

pub struct H264 {
    nal_buf: Vec<u8>,
    timestamp: Option<u32>,
    decoder: ffmpeg_next::decoder::Video,

    needs_keyframe: bool,

    encoder: Option<H264Encoder>,
    pts: i64,
    frame: ffmpeg_next::frame::Video,
}

impl H264 {
    pub fn new() -> Result<Self> {
        let context = Context::new_with_codec(
            decoder::find(Id::H264)
                .ok_or_else(|| anyhow!("can't find h264 codec"))?,
        );
        let decoder = context.decoder().video()?;

        let encoder = Context::new_with_codec(
            ffmpeg_next::encoder::find(Id::H264)
                .ok_or_else(|| anyhow!("can't find h264 codec"))?,
        )
        .encoder()
        .video()?;

        Ok(Self {
            nal_buf: Vec::new(),
            timestamp: None,
            decoder,
            encoder: Some(H264Encoder::Unopened(encoder)),
            // always start with need a keyframe
            needs_keyframe: true,
            pts: 0,
            frame: ffmpeg_next::frame::Video::empty(),
        })
    }
}

impl VideoCodec for H264 {
    fn request_keyframe(&mut self) {
        self.needs_keyframe = true;
    }

    fn encode(
        &mut self,
        src_frame: ffmpeg_next::frame::Video,
    ) -> Result<Vec<Vec<u8>>> {
        let width = src_frame.width();
        let height = src_frame.height();

        if let Some(H264Encoder::Opened(encoder)) = self.encoder.as_ref() {
            if encoder.width() != width || encoder.height() != height {
                self.encoder = Some(H264Encoder::Unopened(
                    Context::new_with_codec(
                        ffmpeg_next::encoder::find(Id::H264)
                            .ok_or_else(|| anyhow!("can't find h264 codec"))?,
                    )
                    .encoder()
                    .video()?,
                ));
            }
        }

        let encoder = if let Some(H264Encoder::Opened(encoder)) =
            self.encoder.as_mut()
        {
            encoder
        } else if let Some(H264Encoder::Unopened(mut encoder)) = self.encoder.take()
        {
            encoder.set_width(width);
            encoder.set_height(height);
            encoder.set_gop(1000);
            encoder.set_max_b_frames(0);
            encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
            encoder.set_colorspace(ffmpeg_next::color::Space::RGB);
            encoder.set_color_range(ffmpeg_next::color::Range::JPEG);
            encoder.set_time_base((1, 30));

            unsafe {
                if let Some(encoder) = encoder.as_mut_ptr().as_mut() {
                    encoder.level = 31;
                    encoder.profile = ffmpeg_next::ffi::FF_PROFILE_H264_BASELINE;
                    encoder.qcompress = 0.6;
                    encoder.keyint_min = 1000;
                }
            }

            let mut option = ffmpeg_next::Dictionary::new();
            option.set("preset", "veryfast");
            option.set("intra-refresh", "1");
            option.set("tune", "zerolatency");
            let encoder = encoder.open_with(option)?;
            self.encoder = Some(H264Encoder::Opened(encoder));
            let Some(H264Encoder::Opened(encoder)) = self.encoder.as_mut() else {
                return Err(anyhow!("no encoder"));
            };
            encoder
        } else {
            return Err(anyhow!("no encoder"));
        };

        if self.frame.width() != width || self.frame.height() != height {
            self.frame = ffmpeg_next::frame::Video::empty();
            self.frame.set_width(width);
            self.frame.set_height(height);
            self.frame.set_format(src_frame.format());
        }
        self.frame.set_pts(Some(self.pts));
        self.pts += 1;

        unsafe {
            let src_ptr = src_frame.as_ptr();
            let ptr = self.frame.as_mut_ptr();
            if let Some(ptr) = ptr.as_mut() {
                if let Some(src_ptr) = src_ptr.as_ref() {
                    ptr.data = src_ptr.data;
                    ptr.linesize = src_ptr.linesize;
                    if self.needs_keyframe {
                        ptr.key_frame = 1;
                        ptr.pict_type = AVPictureType::AV_PICTURE_TYPE_I;
                    }
                }
            }
        }

        encoder.send_frame(&self.frame)?;

        let mut payloads = Vec::new();
        let mut packet = Packet::empty();
        while encoder.receive_packet(&mut packet).is_ok() {
            if let Some(data) = packet.data() {
                let data = SplitBySliceIter::new(data);
                for batch in data {
                    if batch.len() > 1205 {
                        let start = batch[0];
                        let nal_type = start & 0x1f;
                        let nri = start & 0x60;
                        let first = nri | 28;
                        let batch = &batch[1..];

                        let mut chunks = batch.chunks(1200).enumerate().peekable();
                        while let Some((i, chunk)) = chunks.next() {
                            let mut second = nal_type;

                            if i == 0 {
                                second = 0x80 | nal_type;
                            }
                            if chunks.peek().is_none() {
                                second = 0x40 | nal_type;
                            }
                            let mut batch = vec![first, second];
                            batch.extend_from_slice(chunk);
                            payloads.push(batch);
                        }
                    } else {
                        payloads.push(batch.to_vec());
                    }
                }
            }
        }

        if self.needs_keyframe {
            unsafe {
                if let Some(ptr) = self.frame.as_mut_ptr().as_mut() {
                    ptr.key_frame = 0;
                    ptr.pict_type = AVPictureType::AV_PICTURE_TYPE_NONE;
                    self.needs_keyframe = false;
                }
            }
        }

        Ok(payloads)
    }

    fn decode(
        &mut self,
        pkt: &[u8],
        timestamp: u32,
        marker: bool,
    ) -> Result<Vec<ffmpeg_next::frame::Video>> {
        if pkt.is_empty() {
            return Err(anyhow!("pkt is empty"));
        }

        if self.nal_buf.is_empty() {
            self.timestamp = Some(timestamp);
        }
        if self.timestamp != Some(timestamp) {
            return Err(anyhow!("timestamp not the same, packet loss happened"));
        }

        let nalu_idc = (pkt[0] & 0x60) >> 5;
        let nalu_type = pkt[0] & 0x1f;
        let marker = if (nalu_type == 7 || nalu_type == 8) && marker {
            // fix for phones sending sps/pps with maker set to true such as grandstream
            false
        } else {
            marker
        };

        if nalu_type == 28 {
            // FU-A

            let start = pkt[1] & 0x80;
            let end = if marker { 1 } else { pkt[1] & 0x40 };

            let nalu_type = pkt[1] & 0x1f;

            if start > 0 && end > 0 {
                // the fragmentation packet can't be the start and end at the same time
                return Err(anyhow!(
                    "FU-A packet be start and end at the same time"
                ));
            }

            if start > 0 {
                let nalu_type = nalu_type | (nalu_idc << 5);
                self.nal_buf.extend_from_slice(&[0, 0, 0, 1, nalu_type]);
            }
            self.nal_buf.extend_from_slice(&pkt[2..]);
        } else if nalu_type == 24 {
            // STAP-A

            let mut i = 1;
            let mut left = pkt.len() - 1;
            while left > 2 {
                let nalu_size = BigEndian::read_u16(&pkt[i..]);
                i += 2;
                left -= 2;

                if nalu_size as usize > left {
                    return Err(anyhow!("invalid STAP-A packet"));
                }

                // let nalu_hdr = pkt[i];
                // let nalu_type = nalu_hdr & 0x1f;

                self.nal_buf.extend_from_slice(&[0, 0, 0, 1]);
                self.nal_buf
                    .extend_from_slice(&pkt[i..i + nalu_size as usize]);
                i += nalu_size as usize;
                left = left.saturating_sub(nalu_size as usize);
            }
        } else {
            self.nal_buf.extend_from_slice(&[0, 0, 0, 1]);
            self.nal_buf.extend_from_slice(pkt);
        }

        if !marker {
            return Err(anyhow!("video payload not finished yet"));
        }

        // we've got marker, so the packet is ready to decode
        let pkt = Packet::copy(&self.nal_buf);
        self.nal_buf.clear();

        self.decoder.send_packet(&pkt)?;

        let mut frames = Vec::new();
        let mut temp = ffmpeg_next::frame::Video::empty();

        while let Ok(()) = self.decoder.receive_frame(&mut temp) {
            frames.push(temp.clone());
            temp = ffmpeg_next::frame::Video::empty();
        }

        Ok(frames)
    }
}

struct SplitBySliceIter<'a> {
    n: usize,
    data: &'a [u8],
}

impl<'a> SplitBySliceIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { n: 0, data }
    }
}

impl<'a> Iterator for SplitBySliceIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let mut n = self.n;
        let mut start = self.n;
        let mut data = self.data.iter().skip(self.n).multipeek();
        while let Some(i) = data.next() {
            let found = if *i == 0 && data.peek() == Some(&&0) {
                let next = data.peek();
                if next == Some(&&1) {
                    Some(3)
                } else if next == Some(&&0) && data.peek() == Some(&&1) {
                    Some(4)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(found) = found {
                if n == start {
                    start = n + found;
                } else {
                    self.n = n + found;
                    return Some(&self.data[start..n]);
                }
                data.nth(found - 2);
                n += found;
            } else {
                n += 1;
            }
        }
        if start < n {
            self.n = n;
            return Some(&self.data[start..n]);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::h264::SplitBySliceIter;

    #[test]
    fn test_split_by_slice() {
        let data = &[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 2, 2, 2, 2,
            2, 2,
        ];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next().unwrap(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(data.next().unwrap(), &[1, 1, 1, 1, 1, 1]);
        assert_eq!(data.next().unwrap(), &[2, 2, 2, 2, 2, 2]);
        assert_eq!(data.next(), None);

        let data = &[
            0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 2, 2, 2, 2,
            2, 2, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next().unwrap(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(data.next().unwrap(), &[1, 1, 1, 1, 1, 1]);
        assert_eq!(data.next().unwrap(), &[2, 2, 2, 2, 2, 2]);
        assert_eq!(data.next(), None);

        let data = &[
            0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1,
            0, 0, 0, 1, 2, 2, 2, 2, 2, 2,
        ];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next().unwrap(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(data.next().unwrap(), &[1, 1, 1, 1, 1, 1]);
        assert_eq!(data.next().unwrap(), &[2, 2, 2, 2, 2, 2]);
        assert_eq!(data.next(), None);

        let data = &[
            0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1,
            0, 0, 0, 1, 2, 2, 2, 2, 2, 2, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next().unwrap(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(data.next().unwrap(), &[1, 1, 1, 1, 1, 1]);
        assert_eq!(data.next().unwrap(), &[2, 2, 2, 2, 2, 2]);
        assert_eq!(data.next(), None);

        let data = &[
            0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0,
            0, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 2, 2, 2, 2,
            2, 2, 0, 0, 0, 1, 0, 0, 0, 1,
        ];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next().unwrap(), &[0, 0, 0, 0, 0, 0]);
        assert_eq!(data.next().unwrap(), &[1, 1, 1, 1, 1, 1]);
        assert_eq!(data.next().unwrap(), &[2, 2, 2, 2, 2, 2]);
        assert_eq!(data.next(), None);

        let data = &[];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next(), None);

        let data = &[0, 0, 0, 1];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next(), None);

        let data = &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        let mut data = SplitBySliceIter::new(data);
        assert_eq!(data.next(), None);
    }
}
