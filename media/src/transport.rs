use crate::peer_connection::PeerConnectionPool;
use crate::session::NewSessionPool;
use crate::stream::{AudioStreamSenderPool, StreamSenderPoolMessage};
use anyhow::{Error, Result};
use nebula_rpc::message::RpcMessage;
use nebula_rpc::server::RpcEntry;
use std::process::Command;
use std::str;
use tokio;
// use tokio::prelude::*;
use tokio::sync::mpsc::{unbounded_channel, Receiver, UnboundedSender};

pub struct Transport {
    pub pool: AudioStreamSenderPool,
}

impl Transport {
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

    pub async fn run(&mut self) {
        let mut session_pool = NewSessionPool::new(self.pool.sender.clone()).await;
        tokio::spawn(async move {
            session_pool.run().await;
        });
        self.pool.run().await;
    }
}

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
