use anyhow::Result;

pub mod dtmf;
mod fax;
pub mod g722;
pub mod h264;
mod noise;
mod opus;
pub mod pcma;
pub mod pcmu;
pub mod vp8;

pub use fax::Fax;
pub use fax::InitT38Params;
pub use g722::*;
pub use noise::Noise;
pub use opus::Opus;

pub trait Codec: Send {
    fn encode(&mut self, src: &[i16], dst: &mut [u8]) -> Result<usize>;
    fn decode(&mut self, src: &[u8], dst: &mut [i16]) -> Result<usize>;
}

pub trait VideoCodec: Send {
    fn request_keyframe(&mut self);
    fn encode(&mut self, frame: ffmpeg_next::frame::Video) -> Result<Vec<Vec<u8>>>;
    fn decode(
        &mut self,
        pkt: &[u8],
        timestamp: u32,
        marker: bool,
    ) -> Result<Vec<ffmpeg_next::frame::Video>>;
}
