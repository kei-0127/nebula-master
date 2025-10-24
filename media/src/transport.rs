//! # Transport Layer Implementation
//! 
//! This module provides the transport layer for media communication,
//! coordinating peer connections, audio streams, and session management.
//! It serves as the central coordinator for all transport-related operations.
//! 
//! ## Key Features
//! 
//! - **Peer Connection Management**: Manage WebRTC peer connections
//! - **Audio Stream Coordination**: Coordinate audio stream processing
//! - **Session Management**: Handle media session lifecycle
//! - **Transport Coordination**: Coordinate transport layer operations
//! - **Resource Management**: Manage transport resources and pools
//! 
//! ## Transport Components
//! 
//! - **Peer Connection Pool**: Manages WebRTC peer connections
//! - **Audio Stream Pool**: Manages audio stream processing
//! - **Session Pool**: Manages media sessions
//! - **UDP Socket**: Network communication socket
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_media::transport::Transport;
//! 
//! // Create transport layer
//! let mut transport = Transport::new().await?;
//! 
//! // Run transport operations
//! transport.run().await;
//! ```

use crate::peer_connection::PeerConnectionPool;
use crate::session::NewSessionPool;
use crate::stream::AudioStreamSenderPool;
use anyhow::{Error, Result};
use std::process::Command;
use tokio::sync::mpsc::unbounded_channel;

/// Orchestrates peer connections, session pool, and the audio sender pool.
pub struct Transport {
    pub pool: AudioStreamSenderPool,  // Audio stream sender pool
}

impl Transport {
    /// Wire up peer connection pool and audio sender pool; returns a ready `Transport`.
    pub async fn new() -> Result<Transport, Error> {
        let (stream_sender_pool_sender, stream_sender_pool_receiver) =
            unbounded_channel();

        let mut peer_connection_pool =
            PeerConnectionPool::new(stream_sender_pool_sender.clone())
                .await
                .unwrap();
        let peer_connection_udp_socket = peer_connection_pool.udp_socket();
        tokio::spawn(async move {
            peer_connection_pool.run().await;
        });

        let stream_sender_pool = AudioStreamSenderPool::new(
            stream_sender_pool_sender.clone(),
            stream_sender_pool_receiver,
            peer_connection_udp_socket,
        )
        .await?;

        Ok(Transport {
            pool: stream_sender_pool,
        })
    }

    /// Start the session pool and run the audio sender pool.
    pub async fn run(&mut self) {
        let mut session_pool = NewSessionPool::new(self.pool.sender.clone()).await;
        tokio::spawn(async move {
            session_pool.run().await;
        });
        self.pool.run().await;
    }
}

/// Best-effort local IP discovery via `hostname -I` (Linux); returns the first address.
pub fn get_local_ip() -> Option<String> {
    let output = match Command::new("hostname").args(&["-I"]).output() {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    let stdout = match String::from_utf8(output.stdout) {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    let ips: Vec<&str> = stdout.trim().split(" ").collect::<Vec<&str>>();
    let first = ips.first();
    match first {
        Some(first) => {
            if !first.is_empty() {
                return Some(first.to_string());
            } else {
                None
            }
        }
        None => None,
    }
}
