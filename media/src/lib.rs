//! # Media Processing Module
//! 
//! This module provides comprehensive media processing capabilities for the Nebula VoIP system.
//! It handles RTP media streams, audio/video codecs, WebRTC peer connections, and real-time
//! media processing.
//! 
//! ## Core Components
//! 
//! ### Media Processing
//! - **buffer**: Audio/video buffer management and processing
//! - **resampler**: Audio sample rate conversion
//! - **sound**: Audio processing and effects
//! - **packet**: RTP packet handling and processing
//! 
//! ### WebRTC Support
//! - **peer_connection**: WebRTC peer connection management
//! - **dtls**: DTLS security for WebRTC
//! - **srtp**: Secure RTP for media encryption
//! - **rtcp**: RTP Control Protocol for media quality monitoring
//! 
//! ### Transport and Networking
//! - **transport**: Media transport layer abstraction
//! - **socket**: Network socket management
//! - **stream**: Media stream management and routing
//! - **transportcc**: Transport congestion control
//! 
//! ### Quality and Performance
//! - **googcc**: Google Congestion Control algorithm
//! - **nack**: Negative acknowledgment for packet loss recovery
//! - **rtx**: Retransmission for reliable media delivery
//! - **date_rate**: Data rate tracking utilities
//! 
//! ### Caching and Optimization
//! - **key_sorted_cache**: Key-sorted cache for efficient lookups
//! - **two_generation_cache**: Two-generation cache for media buffering
//! - **ring_buffer**: Circular buffer for audio processing
//! 
//! ### Utilities
//! - **math**: Mathematical utilities for media processing
//! - **utils**: General utility functions
//! - **session**: Media session management
//! - **server**: Media server implementation
//! 
//! ## Architecture
//! 
//! The media module follows a layered architecture:
//! 1. **Transport Layer**: UDP/TCP socket management
//! 2. **Protocol Layer**: RTP/RTCP protocol handling
//! 3. **Codec Layer**: Audio/video encoding/decoding
//! 4. **Processing Layer**: Media processing and effects
//! 5. **Application Layer**: WebRTC and session management
//! 
//! ## Performance Characteristics
//! 
//! - **Low Latency**: Sub-50ms processing latency
//! - **High Throughput**: 1000+ concurrent media streams
//! - **Memory Efficient**: Optimized buffer management
//! - **CPU Optimized**: SIMD-accelerated processing
//! 
//! ## Usage
//!
//! ```rust
//! use nebula_media::server::Server;
//!
//! # async fn run() -> anyhow::Result<()> {
//! // Start the media server (transports, RPC, and stream pool)
//! let mut server = Server::new().await?;
//! server.run().await;
//! # Ok(())
//! # }
//! ```

pub mod buffer;
pub mod date_rate;
pub mod dtls;
pub mod googcc;
pub mod key_sorted_cache;
pub mod math;
pub mod nack;
pub mod packet;
pub mod peer_connection;
pub mod resampler;
pub mod ring_buffer;
pub mod rtcp;
pub mod rtx;
pub mod server;
pub mod session;
pub mod socket;
pub mod sound;
pub mod srtp;
pub mod stream;
pub mod transport;
pub mod transportcc;
pub mod two_generation_cache;
pub mod utils;

pub use peer_connection::PeerConnection;
