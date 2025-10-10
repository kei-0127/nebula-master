use anyhow::Result;
use byteorder::{ReadBytesExt, BE, LE};
use serde::{Deserialize, Serialize};

pub type TruncatedPictureId = u16;
pub type FullPictureId = u64;
pub type TruncatedTl0PicIdx = u8;
pub type FullTl0PicIdx = u64;

// Can be used for video resolution
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelSize {
    pub width: usize,
    pub height: usize,
}

/// See https://tools.ietf.org/html/rfc7741 for the format.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct ParsedHeader {
    /// Incremented with each video frame. Really a u15.
    /// Used to provide indicate frame order and gaps.
    /// Must be rewritten or cleared when forwarding simulcast.
    pub picture_id: Option<TruncatedPictureId>,

    // /// If false, the frame can be discarded without disrupting future frames.
    // /// There doesn't seem to be any use for this field
    // /// because we don't support dropping frames in the SFU.
    //  referenced: bool,
    //
    /// Incremented with each frame with TemporalLayerId == 0.
    /// Used to indicate temporal layer dependencies.
    /// Frames with TemporalLayerId > 0 refer to frames with TemporalLayerId == 0
    /// either directly or through a frame with one less TemoralLayerId.
    /// Must be rewritten or cleared when forwarding simulcast.
    pub tl0_pic_idx: Option<TruncatedTl0PicIdx>,

    // /// 0 = temporal base layer. Really a u4.
    // /// There doesn't seem to be any use for this field
    // /// because we don't support dropping frames in the SFU.
    //  temporal_layer_id: Option<u8>,

    // /// AKA "layer sync". If true, this frame references temporal layer 0
    // /// even if this frame's temporal_layer_id > 1. If false, this frame
    // /// references a frame with temporal_layer_id-1.
    // /// But there doesn't seem to be any use for this field
    // /// because we don't support dropping frames in the SFU.
    //  references_temporal_layer0_directly: Option<bool>,
    //
    /// Incremented with each key frame. Really a u5.
    /// There doesn't seem to be any use for this field.
    /// key_frame_index: Option<u8>,
    pub is_key_frame: bool,

    /// (width, height). Only included in the header if is_key_frame.
    /// Subsequent frames must have the same resolution.
    /// Really u14s.
    pub resolution: Option<PixelSize>,
}

#[derive(Debug, Eq, PartialEq)]
struct Byte0 {
    has_extensions: bool,
    starts_partition: bool,
    zero_partition_idx: bool,
}

impl Byte0 {
    fn parse(byte0: u8) -> Self {
        Self {
            has_extensions: byte0.ms_bit(0), //   X bit
            //_reserved1: byte0.ms_bit(1),     // R bit
            //_non_ref_frame: byte0.ms_bit(2), // N bit
            starts_partition: byte0.ms_bit(3), // S bit,
            //_reserved2: byte0.ms_bit(4),     // R bit
            zero_partition_idx: byte0 & 0b111 == 0,
        }
    }

    /// Note that the payload header is present only in packets that have the S bit equal to one
    /// and the PID equal to zero in the payload descriptor.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc7741#section-4.3
    fn has_payload_header(&self) -> bool {
        self.starts_partition && self.zero_partition_idx
    }
}

#[derive(Debug, Eq, PartialEq)]
struct XByte {
    has_picture_id: bool,
    has_tl0_pic_idx: bool,
    has_tid: bool,
    has_key_idx: bool,
}

impl XByte {
    fn parse(x_byte: u8) -> Self {
        Self {
            has_picture_id: x_byte.ms_bit(0),  // I bit
            has_tl0_pic_idx: x_byte.ms_bit(1), // L bit
            has_tid: x_byte.ms_bit(2),         // T bit
            has_key_idx: x_byte.ms_bit(3),     // K bit
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PayloadHeader {
    key_frame: bool,
}

impl PayloadHeader {
    fn parse(byte: u8) -> Self {
        Self {
            key_frame: !byte.ms_bit(7), // P bit: Inverse key frame flag.
        }
    }
}

impl ParsedHeader {
    /// This reads both the "descriptor" and the "header"
    /// See https://datatracker.ietf.org/doc/html/rfc7741#section-4.2
    pub fn read(mut payload: &[u8]) -> Result<Self> {
        let mut header = Self::default();

        let byte0 = Byte0::parse(payload.read_u8()?);

        if byte0.has_extensions {
            let x_byte = XByte::parse(payload.read_u8()?);

            if x_byte.has_picture_id {
                let mut peek = payload;
                if !peek.read_u8()?.ms_bit(0) {
                    // The spec says it could be 7-bit, but WebRTC only sends 15-bit
                    return Err(anyhow::anyhow!("wrong format"));
                }
                let picture_id_with_leading_bit = payload.read_u16::<BE>()?;
                header.picture_id =
                    Some(picture_id_with_leading_bit & 0b0111_1111_1111_1111);
            }

            if x_byte.has_tl0_pic_idx {
                let tl0_pic_idx = payload.read_u8()?;
                header.tl0_pic_idx = Some(tl0_pic_idx);
            };

            if x_byte.has_tid || x_byte.has_key_idx {
                let _tk_byte = payload.read_u8()?;
                // If in the future we want the TID or key frame index, here is how to get it:
                // if has_tid {
                //     header.temporal_layer_id = Some(tk_byte >> 6);
                //     header.references_temporal_layer0_directly = Some(tk_byte.ms_bit(2));
                // }
                // if has_key_idx {
                //     header.key_frame_index = Some(tk_byte & 0b0001_1111);
                // }
            };
        }

        if byte0.has_payload_header() {
            // The codec bitstream format specifies two different variants of the uncompressed data
            // chunk: a 3-octet version for interframes and a 10-octet version for key frames.
            // The first 3 octets are common to both variants.
            let mut common_header = payload.read_slice(3)?;
            let payload0 = PayloadHeader::parse(common_header.read_u8()?);
            header.is_key_frame = payload0.key_frame;
            if header.is_key_frame {
                // In the case of a key frame, the remaining 7 octets are considered to be part
                // of the remaining payload in this RTP format.
                let mut additional_key_frame_header = payload.read_slice(7)?;
                header.resolution =
                    Some(ParsedHeader::size_from_additional_key_frame_header(
                        &mut additional_key_frame_header,
                    )?);
            }
        }

        Ok(header)
    }

    /// see https://datatracker.ietf.org/doc/html/rfc6386#section-9.1
    fn size_from_additional_key_frame_header(
        additional_key_frame_header: &mut &[u8],
    ) -> std::io::Result<PixelSize> {
        let _skipped = additional_key_frame_header.read_slice(3)?;
        let width_with_scale = additional_key_frame_header.read_u16::<LE>()?;
        let height_with_scale = additional_key_frame_header.read_u16::<LE>()?;
        let width = width_with_scale & 0b11_1111_1111_1111;
        let height = height_with_scale & 0b11_1111_1111_1111;
        Ok(PixelSize {
            width: width as usize,
            height: height as usize,
        })
    }
}

use std::ops::{BitAnd, BitOr, Shl, Shr};

pub trait Bits: Sized + Copy {
    const BIT_WIDTH: u8 = (std::mem::size_of::<Self>() * 8) as u8;

    /// Returns true iff the bit at the index is one.
    ///
    /// # Arguments
    ///
    /// * `index` - The 0 based index starting at the most significant bit.  
    fn ms_bit(self, index: u8) -> bool;

    /// Sets the bit to one at the index.
    ///
    /// # Arguments
    ///
    /// * `index` - The 0 based index starting at the most significant bit.  
    fn set_ms_bit(self, index: u8) -> Self;

    /// Returns true iff the bit at the index is one.
    ///
    /// # Arguments
    ///
    /// * `index` - The 0 based index starting at the least significant bit.  
    fn ls_bit(self, index: u8) -> bool;

    /// Sets the bit to one at the index.
    ///
    /// # Arguments
    ///
    /// * `index` - The 0 based index starting at the least significant bit.  
    fn set_ls_bit(self, index: u8) -> Self;
}

impl<T> Bits for T
where
    T: Copy
        + Shr<u8, Output = T>
        + Shl<u8, Output = T>
        + BitAnd<T, Output = T>
        + BitOr<T, Output = T>
        + From<u8>
        + Eq,
{
    fn ms_bit(self, index: u8) -> bool {
        assert!(index < Self::BIT_WIDTH);

        self >> (Self::BIT_WIDTH - index - 1) & T::from(1) == T::from(1)
    }

    fn set_ms_bit(self, index: u8) -> Self {
        assert!(index < Self::BIT_WIDTH);

        self | T::from(1) << (Self::BIT_WIDTH - index - 1)
    }

    fn ls_bit(self, index: u8) -> bool {
        assert!(index < Self::BIT_WIDTH);

        self >> index & T::from(1) == T::from(1)
    }

    fn set_ls_bit(self, index: u8) -> Self {
        assert!(index < Self::BIT_WIDTH);

        self | T::from(1) << index
    }
}

pub trait ReadSliceExt: std::io::Read {
    /// Like `std::io::read_exact`, but borrows from `self` instead.
    fn read_slice(&mut self, n: usize) -> std::io::Result<&[u8]>;
}

impl ReadSliceExt for &'_ [u8] {
    fn read_slice(&mut self, n: usize) -> std::io::Result<&[u8]> {
        if self.len() < n {
            Err(std::io::ErrorKind::UnexpectedEof.into())
        } else {
            let (result, rest) = self.split_at(n);
            *self = rest;
            Ok(result)
        }
    }
}

// This assumes that the picture ID and TL0 PIC IDX are present in the packet
// and that the picture ID is of the 15-bit variety.
// If they aren't, the payload will be corrupted
pub fn modify_header(
    rtp_payload: &mut [u8],
    picture_id: TruncatedPictureId,
    tl0_pic_idx: TruncatedTl0PicIdx,
) {
    rtp_payload[2..4]
        .copy_from_slice(&((picture_id | 0b1000_0000_0000_0000).to_be_bytes()));
    rtp_payload[4] = tl0_pic_idx;
}
