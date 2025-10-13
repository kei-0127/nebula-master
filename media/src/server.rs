//! # Media Server Implementation
//! 
//! Main media server for real-time media processing and WebRTC connections.
//! Central coordinator for all media-related operations.

use crate::peer_connection::PeerConnectionEvent;
use crate::stream::StreamSenderPoolMessage;
use crate::transport::Transport;
use crate::PeerConnection;
use anyhow::{anyhow, Error, Result};
use axum::extract::Extension;
use axum::Json;
use futures::StreamExt;
use lazy_static::lazy_static;
use nebula_db::api::Database;
use nebula_rpc::client::SwitchboardRpcClient;
use nebula_rpc::media::MediaRpcClient;
use nebula_rpc::message::*;
use nebula_rpc::server::RpcServer;
use nebula_rpc::{client::RpcClient, server::RpcEntry};
use nebula_utils::get_local_ip;
use serde::Deserialize;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;
use storagev1::StorageClient;
use thiserror::Error;
use tokio::sync::mpsc::{Receiver, UnboundedSender};
use tracing::info;
use uuid::Uuid;

// Global media service instance
lazy_static! {
    pub static ref MEDIA_SERVICE: MediaService = MediaService::new().unwrap();
}

/// Error types for RPC server operations
#[derive(Debug, Error)]
pub enum RpcServerError {
    #[error("no redis host")]
    NoRedisHost,  // Redis host not configured

    #[error("no db host")]
    NoDBHost,     // Database host not configured

    #[error("no ip")]
    NoIP,         // IP address not available
}

/// Media server configuration
/// Contains network settings, service endpoints, and processing parameters
#[derive(Deserialize)]
pub struct Config {
    pub media_ip: Ipv4Addr,           // Media server IP address
    pub redis: String,                 // Redis connection string
    pub db: String,                    // Database connection string
    pub silent_threshold: usize,       // Audio silence detection threshold
    pub media_host: String,            // Media server hostname
    pub switchboard_host: String,      // Switchboard server hostname
}

/// Main media service coordinator
/// Coordinates WebRTC connections, media processing, RPC communication, and storage
pub struct MediaService {
    pub config: Config,                    // Server configuration
    pub media_rpc_client: MediaRpcClient,  // RPC client for media operations
    pub switchboard_rpc_client: SwitchboardRpcClient,
    pub local_ip: String,
    pub storage_client: StorageClient,
    pub db: Database,
}

pub struct Server {
    transport: Transport,
}

impl MediaService {
    pub fn new() -> Result<MediaService> {
        let config = Config::new()?;

        let media_rpc_client = MediaRpcClient::new();
        let switchboard_rpc_client = SwitchboardRpcClient::new();

        let local_ip = get_local_ip().ok_or(anyhow!("can't get local ip"))?;

        let db = Database::new(config.db.as_ref())?;
        let storage_client = StorageClient::new();

        Ok(MediaService {
            config,
            media_rpc_client,
            switchboard_rpc_client,
            storage_client,
            local_ip,
            db,
        })
    }

    pub async fn process_rpc(
        rpc_msg: RpcMessage,
        sender: &UnboundedSender<StreamSenderPoolMessage>,
    ) -> Result<()> {
        // Handle ICE stop command
        if rpc_msg.method == RpcMethod::StopIce {
            let _ = PeerConnection::new_event(
                &rpc_msg.id,
                PeerConnectionEvent::Stop,
                "stop",
            )
            .await;
            return Ok(());
        }

        // Forward RPC message to stream sender pool
        if let Ok(uuid) = Uuid::from_str(&rpc_msg.id) {
            if sender
                .send(StreamSenderPoolMessage::Rpc(uuid, rpc_msg))
                .is_err()
            {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn new() -> Result<Config> {
        // Read configuration from file
        let contents = fs::read_to_string("/etc/nebula/nebula.conf")?;
        Ok(toml::from_str(&contents)?)
    }
}

impl Server {
    pub async fn new() -> Result<Server, Error> {
        // Initialize transport layer
        let transport = Transport::new().await?;

        Ok(Server { transport })
    }

    pub async fn run(&mut self) {
        info!("media server run");
        async fn health_check() -> &'static str {
            "ok"
        }

        async fn stream_new_message(
            Json(rpc_msg): Json<RpcMessage>,
        ) -> Result<(), axum::http::StatusCode> {
            tokio::spawn(async move {
                let _ = RpcClient::send_to_stream(MEDIA_STREAM, &rpc_msg).await;
            });
            Ok(())
        }

        tokio::spawn(async move {
            axum::Server::bind(&SocketAddr::from_str("0.0.0.0:8123").unwrap())
                .serve(
                    axum::Router::new()
                        .route("/health-check", axum::routing::get(health_check))
                        .route(
                            "/internal/stream",
                            axum::routing::post(stream_new_message),
                        )
                        .into_make_service(),
                )
                .await
                .unwrap();
        });

        // let rt = tokio::runtime::Builder::new_multi_thread()
        //     .enable_all()
        //     .thread_name("main")
        //     .build()
        //     .unwrap();
        // for i in 0..100 {
        //     rt.spawn(async move {
        //         let mut ticker = tokio_timerfd::Interval::new_interval(
        //             std::time::Duration::from_millis(20),
        //         )
        //         .unwrap();
        //         loop {
        //             let start = std::time::Instant::now();
        //             ticker.next().await;
        //             let d = start.elapsed().as_millis();
        //             if d < 19 || d > 20 {
        //                 info!("timer is {d} on");
        //             }
        //         }
        //     });
        // }

        self.transport.run().await
    }
}
