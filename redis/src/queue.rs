use std::{collections::VecDeque, time::Instant};

use redis::aio::Connection;
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedSender},
    oneshot::{self, Receiver, Sender},
};

enum QueueMsg {
    ConnBack(Connection),
    Pop(Sender<Option<Connection>>),
}

pub struct ConnQueue {
    tx: UnboundedSender<QueueMsg>,
}

impl ConnQueue {
    pub fn new() -> Self {
        let max_len = 1000;
        let (tx, mut rx) = unbounded_channel();

        let mut conns: VecDeque<(Connection, Instant)> = VecDeque::new();
        tokio::spawn(async move {
            loop {
                while let Some(msg) = rx.recv().await {
                    match msg {
                        QueueMsg::ConnBack(conn) => {
                            if conns.len() > max_len {
                                continue;
                            }
                            conns.push_back((conn, Instant::now()));
                        }
                        QueueMsg::Pop(sender) => {
                            let conn = conns.pop_back().map(|(conn, _)| conn);
                            let _ = sender.send(conn);
                            if let Some((conn, last_used)) = conns.pop_front() {
                                if last_used.elapsed().as_secs() < 10 {
                                    conns.push_front((conn, last_used));
                                }
                            }
                        }
                    }
                }
            }
        });

        Self { tx }
    }

    pub async fn pop(&self) -> Option<Connection> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(QueueMsg::Pop(tx));
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(1000)) => {
                None
            }
            result = rx => {
                result.ok().flatten()
            }
        }
    }

    pub fn push(&self, conn: Connection) {
        let _ = self.tx.send(QueueMsg::ConnBack(conn));
    }
}
