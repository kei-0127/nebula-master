mod queue;

use anyhow::{anyhow, Result};
use consistent_hash_ring::{Ring, RingBuilder};
use crossbeam::queue::ArrayQueue;
use itertools::Itertools;
use lazy_static::lazy_static;
use rand::distributions::Alphanumeric;
use rand::Rng;
pub use redis;
use redis::aio::Connection;
use redis::{Client, ErrorKind, RedisError};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, time};
use tokio::sync::RwLock;
use tracing::warn;

lazy_static! {
    pub static ref REDIS: RedisPool = RedisPool::new_nebula().unwrap();
}

#[derive(Deserialize)]
pub struct ClusterConfig {
    pub nodes: Vec<NodeConfig>,
}

#[derive(Deserialize)]
pub struct NodeConfig {
    pub addr: String,
    pub zone: String,
}

pub struct Zone {
    nodes: Vec<Node>,
    ring: Ring<usize>,
}

pub struct Node {
    zone: String,
    conns: ArrayQueue<Connection>,
    client: Arc<Client>,
}

async fn get_connection(client: &Client, ms: u64) -> Result<Connection> {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => {
            Err(anyhow!("connection timeout"))
        }
        result = client.get_async_connection() => {
            let connection = result?;
            Ok(connection)
        }
    }
}

impl Node {
    pub fn new(zone: String, addr: String) -> Result<Self> {
        let client = Arc::new(Client::open(addr)?);
        let conns = ArrayQueue::new(100);
        Ok(Self {
            zone,
            conns,
            client,
        })
    }
}

#[derive(Clone)]
pub struct RedisPool {
    zones: Arc<HashMap<String, Zone>>,
    ring: Arc<RwLock<Ring<String>>>,
}

pub struct ClusterConnection {
    conn: Option<Connection>,
    in_query: bool,
    had_error: bool,
    zone: String,
    node: usize,
}

impl Drop for ClusterConnection {
    fn drop(&mut self) {
        if self.in_query || self.had_error {
            // if the connection is still in a query,
            // and it hasn't got a response yet,
            // don't put it back to the pool
            //
            // if the connection had io error,
            // don't put it back to the pool
            return;
        }
        let conn = self.conn.take().unwrap();
        let zone = self.zone.clone();
        let i = self.node;

        if let Some(zone) = REDIS.zones.get(&zone) {
            if let Some(node) = zone.nodes.get(i) {
                let _ = node.conns.push(conn);
            }
        }
    }
}

impl RedisPool {
    pub fn new_nebula() -> Result<RedisPool> {
        let redis_conf = std::env::var("REDIS_CONF")
            .unwrap_or_else(|_| "/etc/nebula/proton.conf".to_string());
        let contents = fs::read_to_string(&redis_conf)?;
        let config: ClusterConfig = toml::from_str(&contents)?;

        let mut zones = HashMap::new();
        let mut ring = RingBuilder::default().vnodes(100).build();

        for node in config.nodes.iter() {
            if !zones.contains_key(&node.zone) {
                zones.insert(
                    node.zone.clone(),
                    Zone {
                        nodes: Vec::new(),
                        ring: RingBuilder::default().vnodes(100).build(),
                    },
                );
                ring.insert(node.zone.clone());
            }
            let zone = zones.get_mut(&node.zone).unwrap();
            let node = Node::new(node.zone.clone(), node.addr.clone())?;
            zone.nodes.push(node);
            zone.ring.insert(zone.nodes.len() - 1);
        }

        if !zones
            .iter()
            .map(|(_zone_name, zone)| zone.nodes.len())
            .all_equal()
        {
            return Err(anyhow!("nodes number needs to be identical across zones"));
        }

        let pool = RedisPool {
            zones: Arc::new(zones),
            ring: Arc::new(RwLock::new(ring)),
        };
        let local_pool = pool.clone();
        tokio::spawn(async move {
            local_pool.monitor().await;
        });
        Ok(pool)
    }

    async fn monitor(&self) {
        let zones = self
            .zones
            .iter()
            .map(|(zone_name, zone)| {
                (
                    zone_name.clone(),
                    zone.nodes
                        .iter()
                        .map(|n| n.client.clone())
                        .collect::<Vec<Arc<Client>>>(),
                )
            })
            .collect::<Vec<(String, Vec<Arc<Client>>)>>();

        let mut zones_active: HashMap<String, bool> = zones
            .iter()
            .map(|(zone_name, _)| (zone_name.to_string(), true))
            .collect();

        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            interval.tick().await;

            for (zone_name, nodes) in zones.iter() {
                let ring = self.ring.clone();
                let zone_name = zone_name.clone();
                let nodes = nodes.clone();

                let mut handles = Vec::new();
                for node in nodes {
                    let job = tokio::spawn(async move {
                        let mut n = 0;
                        loop {
                            if check_node(&node).await.unwrap_or(false) {
                                return Ok(true);
                            }
                            n += 1;
                            if n > 3 {
                                return Err(anyhow!("this connection is dead"));
                            }
                        }
                    });
                    handles.push(job);
                }

                let mut results = Vec::new();
                for job in handles {
                    results.push(job.await);
                }

                let active = results.iter().all(|r| {
                    *r.as_ref().unwrap_or(&Ok(false)).as_ref().unwrap_or(&false)
                });

                let current_active = *zones_active.get(&zone_name).unwrap();
                if current_active != active {
                    *zones_active.get_mut(&zone_name).unwrap() = active;
                    warn!("zone {} active changed to {}", zone_name, active);
                    let mut ring = ring.write().await;
                    if active {
                        ring.insert(zone_name.clone());
                    } else {
                        ring.remove(&zone_name);
                    }
                }
            }
        }
    }

    pub async fn get_conn(&self, key: &str) -> Result<ClusterConnection> {
        let ring = self.ring.read().await;
        if ring.len() == 0 {
            return Err(anyhow!("don't have active zone"));
        }
        let zone_name = ring.get(key);
        let zone = self.zones.get(zone_name).unwrap();
        let i = zone.ring.get(key);
        let node = &zone.nodes[*i];

        let conn = node.conns.pop();
        let conn = if let Some(conn) = conn {
            conn
        } else {
            get_connection(&node.client, 1000).await?
        };
        Ok(ClusterConnection {
            conn: Some(conn),
            in_query: false,
            had_error: false,
            zone: zone_name.to_string(),
            node: *i,
        })
    }

    pub async fn query<T: redis::FromRedisValue>(
        &self,
        cmd: &str,
        key: &str,
        args: &[&str],
        replicate: bool,
    ) -> Result<T> {
        let (result, conn) = self.do_query(cmd, key, args).await;
        if replicate && result.is_ok() {
            if let Some(conn) = conn {
                for name in self.zones.keys() {
                    if &conn.zone != name {
                        let node_index = conn.node;
                        let zone_name = name.clone();
                        let cmd = cmd.to_string();
                        let args: Vec<String> =
                            args.iter().map(|arg| arg.to_string()).collect();
                        tokio::spawn(async move {
                            {
                                if REDIS
                                    .ring
                                    .read()
                                    .await
                                    .weight(&zone_name)
                                    .is_none()
                                {
                                    return;
                                }
                            }
                            let zone = REDIS.zones.get(&zone_name).unwrap();
                            let node = &zone.nodes[node_index];
                            let conn = node.conns.pop();
                            let conn = if let Some(conn) = conn {
                                conn
                            } else {
                                let Ok(conn) =
                                    get_connection(&node.client, 1000).await
                                else {
                                    return;
                                };
                                conn
                            };
                            let mut conn = ClusterConnection {
                                conn: Some(conn),
                                in_query: false,
                                had_error: false,
                                zone: zone_name.clone(),
                                node: node_index,
                            };
                            conn.in_query = true;
                            let result: Result<T, RedisError> = redis::cmd(&cmd)
                                .arg(args)
                                .query_async(conn.conn.as_mut().unwrap())
                                .await;
                            conn.in_query = false;
                            if let Err(e) = result.as_ref() {
                                if e.is_io_error()
                                    || e.kind() == ErrorKind::BusyLoadingError
                                {
                                    conn.had_error = true;
                                }
                            }
                        });
                    }
                }
            }
        }
        result
    }

    async fn do_query<T: redis::FromRedisValue>(
        &self,
        cmd: &str,
        key: &str,
        args: &[&str],
    ) -> (Result<T>, Option<ClusterConnection>) {
        let mut n = 0;
        let mut m = 0;
        loop {
            if let Ok(mut conn) = self.get_conn(key).await {
                conn.in_query = true;
                let result = redis::cmd(cmd)
                    .arg(args)
                    .query_async(conn.conn.as_mut().unwrap())
                    .await;
                conn.in_query = false;

                if let Err(e) = result.as_ref() {
                    if e.is_io_error() || e.kind() == ErrorKind::BusyLoadingError {
                        conn.had_error = true;
                    } else {
                        return (result.map_err(|e| anyhow!(e)), Some(conn));
                    }
                } else {
                    return (result.map_err(|e| anyhow!(e)), Some(conn));
                }
            }

            if m > 100 {
                return (
                    Err(anyhow!("redis retried more than 100 times, quit")),
                    None,
                );
            }
            if n > 10 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                n = 0;
            }

            m += 1;
            n += 1;
        }
    }

    pub async fn xack(&self, stream: &str, group: &str, id: &str) -> Result<usize> {
        self.query("XACK", stream, &[stream, group, id], false)
            .await
    }

    pub async fn xread_timeout<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        id: &str,
        timeout: usize,
        count: usize,
    ) -> Result<T> {
        self.query(
            "XREAD",
            stream,
            &[
                "block",
                &timeout.to_string(),
                "count",
                &count.to_string(),
                "STREAMS",
                stream,
                id,
            ],
            false,
        )
        .await
    }

    pub async fn xread_next_entry_timeout(
        &self,
        stream: &str,
        key_id: &str,
        timeout: usize,
    ) -> Result<(String, String, String)> {
        let streams: Vec<redis::Value> =
            self.xread_timeout(stream, key_id, timeout, 1).await?;
        if streams.is_empty() {
            return Err(anyhow!("no streams found"));
        }
        let (_stream_name, entries): (String, Vec<redis::Value>) =
            redis::from_redis_value(&streams[0])?;
        for entry in entries {
            let (entry_id, entry_key_values): (String, Vec<String>) =
                redis::from_redis_value(&entry)?;
            if let Some((key, value)) = entry_key_values.iter().next_tuple() {
                return Ok((entry_id, key.clone(), value.clone()));
            }
        }
        Err(anyhow!("not a valid event"))
    }

    pub async fn xgroup_create(&self, stream: &str, group: &str) -> Result<String> {
        self.query(
            "XGROUP",
            stream,
            &["CREATE", stream, group, "$", "MKSTREAM"],
            true,
        )
        .await
    }

    pub async fn xreadgroup_timeout<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        timeout: usize,
        count: usize,
    ) -> Result<T> {
        self.query(
            "XREADGROUP",
            stream,
            &[
                "block",
                &timeout.to_string(),
                "group",
                group,
                consumer,
                "count",
                &count.to_string(),
                "STREAMS",
                stream,
                ">",
            ],
            false,
        )
        .await
    }

    pub async fn xrevrange<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        start: &str,
        end: &str,
    ) -> Result<T> {
        self.query("XREVRANGE", stream, &[stream, start, end], false)
            .await
    }

    pub async fn xrevrange_count<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        start: &str,
        end: &str,
        count: usize,
    ) -> Result<T> {
        self.query(
            "XREVRANGE",
            stream,
            &[stream, start, end, "COUNT", &count.to_string()],
            false,
        )
        .await
    }

    pub async fn xrange<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        start: &str,
        end: &str,
    ) -> Result<T> {
        self.query("XRANGE", stream, &[stream, start, end], false)
            .await
    }

    pub async fn xadd<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        field: &str,
        value: &str,
    ) -> Result<T> {
        self.query("XADD", stream, &[stream, "*", field, value], false)
            .await
    }

    pub async fn xadd_maxlen<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        field: &str,
        value: &str,
        maxlen: u64,
    ) -> Result<T> {
        self.query(
            "XADD",
            stream,
            &[
                stream,
                "MAXLEN",
                "~",
                &maxlen.to_string(),
                "*",
                field,
                value,
            ],
            false,
        )
        .await
    }

    pub async fn xaddex<T: redis::FromRedisValue>(
        &self,
        stream: &str,
        field: &str,
        value: &str,
        expire: u64,
    ) -> Result<T> {
        let script = r#"
            local res
            res = redis.call('XADD', KEYS[1], '*', ARGV[1], ARGV[2])
            redis.call('EXPIRE', KEYS[1], ARGV[3])
            return res
            "#;
        self.query(
            "EVAL",
            stream,
            &[script, "1", stream, field, value, &expire.to_string()],
            false,
        )
        .await
    }

    pub async fn hsetnx(&self, key: &str, field: &str, value: &str) -> Result<bool> {
        self.query("HSETNX", key, &[key, field, value], true).await
    }

    pub async fn hmset(&self, key: &str, pairs: Vec<(&str, &str)>) -> Result<usize> {
        let mut params = pairs
            .iter()
            .flat_map(|i| vec![i.0, i.1])
            .collect::<Vec<&str>>();
        params.insert(0, key);
        self.query("HSET", key, &params, true).await
    }

    pub async fn hmset_nx_ex(
        &self,
        key: &str,
        expire: i32,
        pairs: Vec<(&str, &str)>,
    ) -> Result<bool> {
        let script = r#"
        if redis.call('exists', KEYS[1]) == 0 then
            redis.call('HMSET', KEYS[1], unpack(ARGV, 2))
            redis.call('EXPIRE', KEYS[1], ARGV[1])
            return 1
        else
            redis.call('EXPIRE', KEYS[1], ARGV[1])
            return 0
        end
        "#;
        let mut params = pairs
            .iter()
            .flat_map(|i| vec![i.0, i.1])
            .collect::<Vec<&str>>();
        let expire = expire.to_string();
        let mut args = vec![script, "1", key, &expire];
        args.append(&mut params);
        self.query("EVAL", key, &args, true).await
    }

    pub async fn hget<T: redis::FromRedisValue>(
        &self,
        key: &str,
        field: &str,
    ) -> Result<T> {
        self.query("HGET", key, &[key, field], false).await
    }

    pub async fn hdel(&self, key: &str, field: &str) -> Result<usize> {
        self.query("HDEL", key, &[key, field], true).await
    }

    pub async fn hset(&self, key: &str, field: &str, value: &str) -> Result<usize> {
        self.query("HSET", key, &[key, field, value], true).await
    }

    pub async fn hsetex(&self, key: &str, field: &str, value: &str) -> Result<bool> {
        let script = r#"
            if redis.call('exists', KEYS[1]) == 1 then
                redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
                return 1
            else
                return 0
            end
            "#;
        self.query("EVAL", key, &[script, "1", key, field, value], true)
            .await
    }

    pub async fn hset_key_ex_field_nx(
        &self,
        key: &str,
        field: &str,
        value: &str,
    ) -> Result<bool> {
        let script = r#"
            if redis.call('exists', KEYS[1]) == 1 then
                return redis.call('HSETNX', KEYS[1], ARGV[1], ARGV[2])
            else
                return 0
            end
            "#;
        self.query("EVAL", key, &[script, "1", key, field, value], true)
            .await
    }

    pub async fn hmget<T: redis::FromRedisValue>(
        &self,
        key: &str,
        fileds: &[&str],
    ) -> Result<T> {
        self.query("HMGET", key, &[&[key], fileds].concat(), false)
            .await
    }

    pub async fn hgetall<T: redis::FromRedisValue>(&self, key: &str) -> Result<T> {
        self.query("HGETALL", key, &[key], false).await
    }

    pub async fn hexists(&self, key: &str, field: &str) -> Result<bool> {
        self.query("HEXISTS", key, &[key, field], false).await
    }

    pub async fn hincrby_key_exits(&self, key: &str, field: &str) -> Result<usize> {
        let script = r#"
            if redis.call('exists', KEYS[1]) == 1 then
                return redis.call('HINCRBY', KEYS[1], ARGV[1], 1)
            else
                return 0
            end
            "#;
        self.query("EVAL", key, &[script, "1", key, field], true)
            .await
    }

    pub async fn srem(&self, key: &str, member: &str) -> Result<usize> {
        self.query("SREM", key, &[key, member], true).await
    }

    pub async fn sadd(&self, key: &str, member: &str) -> Result<usize> {
        self.query("SADD", key, &[key, member], true).await
    }

    pub async fn sismember(&self, key: &str, member: &str) -> Result<bool> {
        self.query("SISMEMBER", key, &[key, member], false).await
    }

    pub async fn smembers<T: redis::FromRedisValue>(&self, key: &str) -> Result<T> {
        self.query("SMEMBERS", key, &[key], false).await
    }

    pub async fn scard(&self, key: &str) -> Result<usize> {
        self.query("SCARD", key, &[key], false).await
    }

    pub async fn lrange<T: redis::FromRedisValue>(
        &self,
        key: &str,
        start: i32,
        end: i32,
    ) -> Result<T> {
        self.query(
            "LRANGE",
            key,
            &[key, &start.to_string(), &end.to_string()],
            false,
        )
        .await
    }

    pub async fn ltrim(&self, key: &str, start: i32, end: i32) -> Result<bool> {
        self.query(
            "LTRIM",
            key,
            &[key, &start.to_string(), &end.to_string()],
            true,
        )
        .await
    }

    pub async fn lpop(&self, key: &str) -> Result<String> {
        self.query("LPOP", key, &[key], true).await
    }

    pub async fn lpush(&self, key: &str, value: &str) -> Result<usize> {
        self.query("LPUSH", key, &[key, value], true).await
    }

    pub async fn rpush(&self, key: &str, element: &str) -> Result<usize> {
        self.query("RPUSH", key, &[key, element], true).await
    }

    pub async fn lrem(&self, key: &str, num: usize, value: &str) -> Result<usize> {
        self.query("LREM", key, &[key, &num.to_string(), value], true)
            .await
    }

    pub async fn zadd(&self, key: &str, score: i64, member: &str) -> Result<usize> {
        self.query("ZADD", key, &[key, &score.to_string(), member], true)
            .await
    }

    pub async fn zrem(&self, key: &str, member: &str) -> Result<usize> {
        self.query("ZREM", key, &[key, member], true).await
    }

    pub async fn zrank(&self, key: &str, member: &str) -> Result<usize> {
        self.query("ZRANK", key, &[key, member], false).await
    }

    pub async fn zcard(&self, key: &str) -> Result<usize> {
        self.query("ZCARD", key, &[key], false).await
    }

    pub async fn zrange(
        &self,
        key: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<String>> {
        self.query(
            "ZRANGE",
            key,
            &[key, &start.to_string(), &end.to_string()],
            false,
        )
        .await
    }

    pub async fn zrange_withscores(
        &self,
        key: &str,
        start: i64,
        end: i64,
    ) -> Result<Vec<String>> {
        self.query(
            "ZRANGE",
            key,
            &[key, &start.to_string(), &end.to_string(), "WITHSCORES"],
            false,
        )
        .await
    }

    pub async fn getset<T: redis::FromRedisValue>(
        &self,
        key: &str,
        value: &str,
    ) -> Result<T> {
        self.query("GETSET", key, &[key, value], true).await
    }

    pub async fn sscan(
        &self,
        key: &str,
        cursor: u64,
        count: usize,
    ) -> Result<(u64, Vec<String>)> {
        let result: Vec<redis::Value> = self
            .query(
                "SSCAN",
                key,
                &[key, &cursor.to_string(), "COUNT", &count.to_string()],
                false,
            )
            .await?;
        let new_cursor: u64 = redis::from_redis_value(
            result.get(0).ok_or(anyhow!("don't have new cursor"))?,
        )?;

        let values: Vec<String> = redis::from_redis_value(
            result.get(1).ok_or(anyhow!("don't have vlaues"))?,
        )?;

        Ok((new_cursor, values))
    }

    pub async fn get<T: redis::FromRedisValue>(&self, key: &str) -> Result<T> {
        self.query("GET", key, &[key], false).await
    }

    pub async fn set(&self, key: &str, value: &str) -> Result<String> {
        self.query("SET", key, &[key, value], true).await
    }

    pub async fn incr(&self, key: &str) -> Result<i64> {
        self.query("INCR", key, &[key], true).await
    }

    pub async fn incr_by(&self, key: &str, n: usize) -> Result<i64> {
        self.query("INCRBY", key, &[key, &n.to_string()], true)
            .await
    }

    pub async fn del(&self, key: &str) -> Result<usize> {
        self.query("DEL", key, &[key], true).await
    }

    pub async fn del_if_value(&self, key: &str, value: &str) -> Result<bool> {
        let script = r#"
            if redis.call("get",KEYS[1]) == ARGV[1] then
               redis.call("del",KEYS[1])
               return 1
            else
               return 0
            end
            "#;
        self.query("EVAL", key, &[script, "1", key, value], true)
            .await
    }

    pub async fn setex(
        &self,
        key: &str,
        expire: u64,
        value: &str,
    ) -> Result<String> {
        self.query("SETEX", key, &[key, &expire.to_string(), value], true)
            .await
    }

    pub async fn setnx(&self, key: &str, value: &str) -> Result<bool> {
        self.query("SETNX", key, &[key, value], true).await
    }

    /// set the key to value with expire time, if the key doesn't exist or the value equals to it
    /// retrun true if value successfully set
    pub async fn setexnx_or_eq(
        &self,
        key: &str,
        value: &str,
        expire: u64,
    ) -> Result<bool> {
        let script = r#"
            if redis.call("EXISTS", KEYS[1]) == 0 then
                redis.call("SETEX", KEYS[1], ARGV[2], ARGV[1])
                return 1
            elseif redis.call("get",KEYS[1]) == ARGV[1] then
                redis.call("expire", KEYS[1], ARGV[2])
                return 1
            else
                return 0
            end
            "#;
        self.query(
            "EVAL",
            key,
            &[script, "1", key, value, &expire.to_string()],
            true,
        )
        .await
    }

    pub async fn get_and_ex(&self, key: &str, expire: u64) -> Result<String> {
        let script = r#"
            if redis.call("EXISTS", KEYS[1]) == 0 then
                return nil
            else
                redis.call("expire", KEYS[1], ARGV[1])
                return redis.call("get", KEYS[1])
            end
            "#;
        self.query("EVAL", key, &[script, "1", key, &expire.to_string()], true)
            .await
    }

    // set the value if it doesn't exit, otherwise set the new expiry
    pub async fn set_or_ex(
        &self,
        key: &str,
        value: &str,
        expire: u64,
    ) -> Result<String> {
        let script = r#"
            if redis.call("EXISTS", KEYS[1]) == 0 then
                redis.call("SETEX", KEYS[1], ARGV[2], ARGV[1])
                return ARGV[1]
            else
                redis.call("expire", KEYS[1], ARGV[2])
                return redis.call("get",KEYS[1])
            end
            "#;
        self.query(
            "EVAL",
            key,
            &[script, "1", key, value, &expire.to_string()],
            true,
        )
        .await
    }

    pub async fn setexnx(&self, key: &str, value: &str, expire: u64) -> bool {
        self.query(
            "SET",
            key,
            &[key, value, "EX", &expire.to_string(), "NX"],
            true,
        )
        .await
        .unwrap_or("".to_string())
            == "OK"
    }

    pub async fn setpxnx(&self, key: &str, value: &str, expire: u64) -> bool {
        self.query(
            "SET",
            key,
            &[key, value, "PX", &expire.to_string(), "NX"],
            true,
        )
        .await
        .unwrap_or("".to_string())
            == "OK"
    }

    pub async fn expire(&self, key: &str, expire: u64) -> Result<bool> {
        self.query("EXPIRE", key, &[key, &expire.to_string()], true)
            .await
    }

    pub async fn ttl(&self, key: &str) -> Result<i64> {
        self.query("TTL", key, &[key], false).await
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        self.query("EXISTS", key, &[key], false).await
    }
}

pub struct DistributedMutex {
    key: String,
    random_string: String,
}

impl Drop for DistributedMutex {
    fn drop(&mut self) {
        self.unlock()
    }
}

impl DistributedMutex {
    pub fn new(key: String) -> DistributedMutex {
        let random_string = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .collect::<String>();

        DistributedMutex { key, random_string }
    }

    async fn acquire(&self) -> bool {
        REDIS.setexnx(&self.key, &self.random_string, 8).await
    }

    pub async fn lock(&self) {
        let start = time::Instant::now();
        loop {
            if self.acquire().await {
                return;
            }

            if start.elapsed().as_secs() > 8 {
                return;
            }

            tokio::time::sleep(time::Duration::from_millis(200)).await;
        }
    }

    pub fn unlock(&self) {
        let key = self.key.clone();
        let random_string = self.random_string.clone();
        tokio::spawn(async move {
            let _ = REDIS.del_if_value(&key, &random_string).await;
        });
    }
}

async fn check_node(node: &Client) -> Result<bool> {
    let mut conn = match get_connection(node, 1000).await {
        Ok(conn) => conn,
        Err(_) => return Err(anyhow!("can't get connection")),
    };
    let cmd = redis::cmd("PING");
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(1000)) => {
            Err(anyhow!("connection timeout"))
        }
        result = cmd.query_async(&mut conn) => {
            let result: String = result?;
            Ok(result == "PONG")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_connection() {
        //Client::open("redis://10.154.0.5:6379").unwrap();
        // RedisPool::new_nebula().unwrap();
    }
}
