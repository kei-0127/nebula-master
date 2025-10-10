use super::message::*;
use anyhow::{Error, Result};
use itertools::Itertools;
use nebula_redis::{
    redis::{self},
    REDIS,
};
use serde_json;
use std::fs;
use tokio;
use tokio::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Debug)]
pub struct RpcEntry {
    entry_id: String,
    stream_name: String,
    stream_group: String,
}

impl RpcEntry {
    pub async fn ack(&self) {
        if self.stream_group != "" {
            let _ = REDIS
                .xack(&self.stream_name, &self.stream_group, &self.entry_id)
                .await;
        }
    }
}

pub struct RpcServer {
    hostname: String,
    sender: Sender<(RpcEntry, RpcMessage)>,
    stream_name: String,
    stream_group: String,
}

impl RpcServer {
    pub async fn new(
        stream_name: String,
        stream_group: String,
    ) -> Receiver<(RpcEntry, RpcMessage)> {
        let hostname = tokio::fs::read_to_string("/proc/sys/kernel/hostname")
            .await
            .unwrap_or("".to_string());
        let (sender, receiver) = mpsc::channel(100);

        let mut rpc_server = RpcServer {
            hostname,
            sender,
            stream_name,
            stream_group,
        };

        tokio::spawn(async move {
            rpc_server.run().await;
        });

        receiver
    }

    async fn run(&mut self) {
        let mut key_id = "$".to_string();
        let sender = self.sender.clone();
        loop {
            let process_key_id = key_id.clone();
            tokio::select! {
                 _ = sender.closed() => {
                      return;
                 }
                 result = self.process_stream(&process_key_id) => {
                      if let Ok(new_key_id) = result {
                          key_id = new_key_id;
                      }
                 }
            }
        }
    }

    async fn process_stream(&mut self, key_id: &str) -> Result<String, Error> {
        let streams: Vec<redis::Value> = if !self.stream_group.is_empty() {
            match REDIS
                .xreadgroup_timeout(
                    &self.stream_name,
                    &self.stream_group,
                    &self.hostname,
                    1000,
                    1000,
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    let _ = REDIS
                        .xgroup_create(&self.stream_name, &self.stream_group)
                        .await;
                    return Err(e);
                }
            }
        } else {
            REDIS
                .xread_timeout(&self.stream_name, key_id, 1000, 1000)
                .await?
        };
        if streams.is_empty() {
            return Err(anyhow::anyhow!("no stream read"));
        }

        let mut new_key_id = "$".to_string();

        for stream in streams {
            let (_stream_name, entries): (String, Vec<redis::Value>) =
                redis::from_redis_value(&stream)?;

            for entry in entries {
                let (entry_id, entry_key_values): (String, Vec<String>) =
                    redis::from_redis_value(&entry)?;
                for (key, value) in entry_key_values.iter().next_tuple() {
                    if key == "message" {
                        let rpc_msg: RpcMessage = serde_json::from_str(value)?;
                        let _ = self
                            .sender
                            .send((
                                RpcEntry {
                                    entry_id: entry_id.clone(),
                                    stream_name: self.stream_name.clone(),
                                    stream_group: self.stream_group.clone(),
                                },
                                rpc_msg,
                            ))
                            .await;
                    }
                }
                new_key_id = entry_id;
            }
        }

        Ok(new_key_id)
    }
}
