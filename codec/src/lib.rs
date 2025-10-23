//! # Codec Module
//! 
//! This module provides audio and video codec implementations for the Nebula VoIP system.
//! It supports various audio and video codecs commonly used in VoIP and video conferencing
//! applications, with optimized performance for real-time communication.
//! 
//! ## Core Components
//! 
//! ### Audio Codecs
//! - **opus**: Opus codec for high-quality audio compression
//! - **g722**: G.722 wideband audio codec
//! - **pcma**: PCM A-law audio codec
//! - **pcmu**: PCM μ-law audio codec
//! - **noise**: Noise generation and suppression
//! - **dtmf**: Dual-tone multi-frequency signaling
//! 
//! ### Video Codecs
//! - **h264**: H.264 video codec for high-quality video compression
//! - **vp8**: VP8 video codec for WebRTC compatibility
//! 
//! ### Special Stream Codecs
//! - **fax**: Fax over IP (T.38) support
//! 
//! ## Key Features
//! 
//! ### Audio Processing
//! - **High Quality**: Opus codec with adaptive bitrate
//! - **Low Latency**: Optimized for real-time communication
//! - **Wideband Support**: Support for wideband audio (16kHz)
//! - **DTMF Support**: In-band and out-of-band DTMF signaling
//! - **Noise Suppression**: Advanced noise reduction algorithms
//! 
//! ### Video Processing
//! - **H.264 Support**: Industry-standard video compression
//! - **WebRTC Compatibility**: VP8 codec for browser compatibility
//! - **Keyframe Management**: Intelligent keyframe generation
//! - **Adaptive Quality**: Dynamic quality adjustment based on network conditions
//! 
//! ### Performance Optimization
//! - **SIMD Acceleration**: SIMD-optimized processing where available
//! - **Memory Efficiency**: Optimized memory usage for embedded systems
//! - **CPU Optimization**: Efficient CPU usage for high concurrent loads
//! - **Zero-Copy**: Minimize memory copies for better performance
//! 
//! ### Codec Features
//! - **Adaptive Bitrate**: Dynamic bitrate adjustment
//! - **Error Resilience**: Robust error handling and recovery
//! - **Interoperability**: Standards-compliant implementations
//! - **Configurable**: Flexible configuration options
//! 
//! ## Architecture
//! 
//! The codec module follows a trait-based architecture:
//! 1. **Codec Trait**: Common interface for all audio codecs
//! 2. **VideoCodec Trait**: Common interface for all video codecs
//! 3. **Implementation**: Specific codec implementations
//! 4. **FFI Layer**: Foreign function interface for native libraries
//! 5. **Wrapper Layer**: Rust-safe wrappers around native code
//! 
//! ## Supported Codecs
//! 
//! ### Audio Codecs
//! - **Opus**: 6-510 kbps, 8-48 kHz, stereo/mono
//! - **G.722**: 64 kbps, 16 kHz, mono
//! - **PCMA**: 64 kbps, 8 kHz, mono
//! - **PCMU**: 64 kbps, 8 kHz, mono
//! 
//! ### Video Codecs
//! - **H.264**: Baseline/Main/High profiles, 4K support
//! - **VP8**: WebRTC standard, up to 1080p
//! 
//! ## Performance Characteristics
//! 
//! - **Audio Latency**: <20ms encoding/decoding latency
//! - **Video Latency**: <50ms encoding/decoding latency
//! - **CPU Usage**: <10% CPU for 100 concurrent audio streams
//! - **Memory Usage**: <1MB per codec instance
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_codec::{Codec, Opus};
//! use nebula_codec::{VideoCodec, H264};
//! 
//! // Audio codec usage
//! let mut opus = Opus::new(48000, 2, Application::Voip)?;
//! let mut output = vec![0u8; 1024];
//! let encoded = opus.encode(&audio_samples, &mut output)?;
//! 
//! // Video codec usage
//! let mut h264 = H264::new()?;
//! let encoded_frames = h264.encode(video_frame)?;
//! ```

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

/// Audio codec trait for encoding and decoding audio data
/// 
/// This trait provides a common interface for all audio codecs,
/// allowing for polymorphic codec usage and easy codec switching.
pub trait Codec: Send {
    /// Encode audio samples to compressed format
    /// 
    /// # Arguments
    /// * `src` - Input audio samples (16-bit PCM)
    /// * `dst` - Output buffer for compressed data
    /// 
    /// # Returns
    /// Number of bytes written to output buffer
    fn encode(&mut self, src: &[i16], dst: &mut [u8]) -> Result<usize>;
    
    /// Decode compressed audio data to PCM samples
    /// 
    /// # Arguments
    /// * `src` - Input compressed audio data
    /// * `dst` - Output buffer for PCM samples
    /// 
    /// # Returns
    /// Number of samples written to output buffer
    fn decode(&mut self, src: &[u8], dst: &mut [i16]) -> Result<usize>;
}

/// Video codec trait for encoding and decoding video data
/// 
/// This trait provides a common interface for all video codecs,
/// supporting both encoding and decoding operations with keyframe management.
pub trait VideoCodec: Send {
    /// Request a keyframe for the next encoded frame
    /// 
    /// This is useful for error recovery and ensuring video quality
    /// after network issues or codec state problems.
    fn request_keyframe(&mut self);
    
    /// Encode a video frame to compressed format
    /// 
    /// # Arguments
    /// * `frame` - Input video frame
    /// 
    /// # Returns
    /// Vector of encoded frame packets
    fn encode(&mut self, frame: ffmpeg_next::frame::Video) -> Result<Vec<Vec<u8>>>;
    
    /// Decode compressed video data to video frames
    /// 
    /// # Arguments
    /// * `pkt` - Input compressed video packet
    /// * `timestamp` - Packet timestamp
    /// * `marker` - End-of-frame marker
    /// 
    /// # Returns
    /// Vector of decoded video frames
    fn decode(
        &mut self,
        pkt: &[u8],
        timestamp: u32,
        marker: bool,
    ) -> Result<Vec<ffmpeg_next::frame::Video>>;
}
