use std::{collections::HashMap, str::FromStr, time::Duration};

use anyhow::{anyhow, Result};
use nebula_db::models::Provider;
use nebula_redis::{DistributedMutex, REDIS};
use sip::{
    message::{Address, Authorization, Cseq, Message, Method, Uri},
    transaction::TM,
};
use tracing::error;

use crate::{auth::Auth, server::PRPOXY_SERVICE};

pub struct ProviderService {
    location: Option<String>,
}

impl ProviderService {
    pub fn new(location: Option<String>) -> Self {
        Self { location }
    }

    pub async fn run(&self) -> Result<()> {
        let location = self
            .location
            .as_ref()
            .ok_or_else(|| anyhow!("provider service needs a location"))?;

        let _ = self.register_providers(location).await;

        Ok(())
    }

    async fn register_providers(&self, location: &str) -> Result<()> {
        println!("register providers");
        loop {
            let providers = PRPOXY_SERVICE
                .db
                .get_registration_providers_at_location(location)
                .await
                .ok_or_else(|| anyhow!("don't have providers in this location"))?;
            for provider in providers {
                tokio::spawn(async move {
                    if let Err(e) = Self::register_provider(provider).await {
                        error!("register provider error: {e}");
                    }
                });
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn register_provider(provider: Provider) -> Result<()> {
        let redis_key = format!("nebula:provider_registration:{}", provider.id);

        let mutex = DistributedMutex::new(format!("{redis_key}:lock"));
        mutex.lock().await;

        let (callid, seq) = {
            if let Ok(ttl) = REDIS.ttl(&redis_key).await {
                if ttl > 10 {
                    // The registration is still more than 10 seconds, so don't need
                    // to do anything
                    return Ok(());
                }
            }

            let result: Result<HashMap<String, String>> =
                REDIS.hgetall(&redis_key).await;

            result
                .ok()
                .as_ref()
                .and_then(|result| {
                    Some((
                        result.get("callid")?.to_string(),
                        result.get("seq")?.parse::<i32>().ok()?,
                    ))
                })
                .unwrap_or_else(|| (nebula_utils::uuid(), 1))
        };

        println!("register provider {:?}", provider);
        let uri = provider
            .uri
            .as_ref()
            .ok_or_else(|| anyhow!("provider doens't have uri"))?;
        let uri = Uri::from_str(uri)?;
        let user = provider
            .user
            .as_ref()
            .ok_or_else(|| anyhow!("provider doens't have user"))?;
        let password = provider
            .password
            .as_ref()
            .ok_or_else(|| anyhow!("provider doens't have password"))?;

        let mut remote_uri = uri.clone();
        remote_uri.is_provider = Some(true);

        let register = Message {
            remote_uri: Some(remote_uri),
            request_uri: Some(uri.clone()),
            callid,
            max_forwards: Some(70),
            method: Some(Method::REGISTER),
            cseq: Cseq {
                seq,
                method: Method::REGISTER,
            },
            expires: Some(300),
            from: Address {
                uri: Uri {
                    user: Some(user.clone()),
                    host: uri.host.clone(),
                    ..Default::default()
                },
                tag: Some(nebula_utils::rand_string(10)),
                ..Default::default()
            },
            to: Address {
                uri: Uri {
                    user: Some(user.clone()),
                    host: uri.host.clone(),
                    ..Default::default()
                },
                ..Default::default()
            },
            allow: Some(
                "SUBSCRIBE, NOTIFY, INVITE, ACK, CANCEL, BYE, REFER, INFO, OPTIONS, MESSAGE"
                    .to_string(),
            ),
            ..Default::default()
        };

        TM.send(&register).await?;

        println!("send register {register}");
        let stream = format!(
            "nebula:register_response:{}:{}",
            register.callid, register.cseq.seq
        );
        let (_, key, value) = REDIS
            .xread_next_entry_timeout(&stream, "0", 10 * 1000)
            .await?;
        if key != "message" {
            return Err(anyhow!("can't receive response message"));
        }
        println!("resp is {}", value);
        let resp = Message::from_str(&value)?;
        println!("resp is {}", resp);

        if resp.code != Some(401) {
            return Err(anyhow!("got wrong response {resp}"));
        }

        let auth = resp
            .www_authenticate
            .as_ref()
            .ok_or_else(|| anyhow!("no www-authenticiate"))?;
        let digest = Auth::digest(
            user,
            password,
            &auth.realm,
            &uri.to_string(),
            &Method::REGISTER.to_string(),
            &auth.nonce,
            None,
            None,
            None,
        );

        let mut register = register.clone();
        register.cseq.seq += 1;

        register.authorization = Some(Authorization {
            username: user.to_string(),
            realm: auth.realm.clone(),
            nonce: auth.nonce.clone(),
            uri: uri.to_string(),
            response: digest,
            cnonce: None,
            qop: None,
            nonce_count: None,
        });

        let tx = TM.send(&register).await?;

        let stream = format!(
            "nebula:register_response:{}:{}",
            register.callid, register.cseq.seq
        );
        let (_, key, value) = REDIS
            .xread_next_entry_timeout(&stream, "0", 10 * 1000)
            .await?;
        if key != "message" {
            return Err(anyhow!("can't receive response message"));
        }
        println!("resp is {}", value);
        let resp = Message::from_str(&value)?;
        println!("resp is {}", resp);

        if resp.code != Some(200) {
            return Err(anyhow!("got wrong response {resp}"));
        }

        let contact = resp
            .contact
            .as_ref()
            .ok_or_else(|| anyhow!("no contact in response"))?;
        let expires = contact
            .expires
            .as_ref()
            .ok_or_else(|| anyhow!("no expires in contact"))?;

        let seq = if register.cseq.seq > 99999 {
            1
        } else {
            register.cseq.seq + 1
        };

        let ip = &tx.get_remote_uri().await?.ip;
        if ip.is_empty() {
            return Err(anyhow!("no ip on remote uri"));
        }
        println!("provider ip is {ip}");
        let cache_key = format!("nebula:cache:provider:{ip}");
        REDIS
            .setex(
                &cache_key,
                *expires as u64,
                &serde_json::to_string(&provider)?,
            )
            .await?;

        REDIS
            .hmset(
                &redis_key,
                vec![("seq", &seq.to_string()), ("callid", &register.callid)],
            )
            .await?;
        REDIS.expire(&redis_key, *expires as u64).await?;

        Ok(())
    }
}
