use anyhow::{anyhow, Error, Result};
use chrono::Utc;
use nebula_redis::REDIS;
use nebula_utils::sha256;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sip::message::{Address, Cseq, Location, Message, Method, Uri, Via};
use sip::transaction::TM;
use sip::transport::TransportType;
use std::collections::HashSet;
use std::str::FromStr;
use std::{collections::HashMap, time::Duration};

use crate::models::User;

pub const USER: &str = "user";
pub const CONTACT: &str = "contact";
pub const RECEIVED: &str = "received";
pub const USER_AGENT: &str = "user_agent";
pub const CALL_ID: &str = "callid";
pub const LOCATION_ID: &str = "location_id";
pub const REGISTER_AT: &str = "registered_at";
pub const EXPIRES_AT: &str = "expires_at";
pub const PROXY_HOST: &str = "proxy_host";

#[derive(Serialize, Deserialize, Clone)]
pub struct UsrlocData {
    pub user: String,
    pub callid: String,
    pub location_id: String,
    pub proxy_host: String,
    pub contact: String,
    pub received: String,
    pub registered_at: String,
    pub expires_at: String,
    pub user_agent: String,
    pub expires: u64,
}

pub struct Usrloc {}

impl Default for Usrloc {
    fn default() -> Self {
        Self::new()
    }
}

impl Usrloc {
    pub fn new() -> Self {
        Usrloc {}
    }

    pub async fn save(&self, proxy_host: &str, req: &Message) -> Option<UsrlocData> {
        let expires = match req.expires {
            Some(e) => e,
            None => match &req.contact {
                Some(c) => c.expires.unwrap_or(0),
                None => 0,
            },
        };

        let authorization = req.authorization.as_ref()?;
        let user = authorization.username.to_lowercase();
        let user = if let Some(prefix) =
            authorization.realm.strip_suffix(".hosted.surevoip.co.uk")
        {
            format!("{user}{}", prefix.to_lowercase())
        } else {
            user
        };
        let hashed_callid = sha256(&req.callid);

        let data = UsrlocData {
            user: user.clone(),
            callid: req.callid.clone(),
            proxy_host: proxy_host.to_string(),
            location_id: hashed_callid.clone(),
            contact: req
                .contact
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "".to_string()),
            received: req
                .remote_uri
                .as_ref()
                .map(|u| u.to_string())
                .unwrap_or_else(|| "".to_string()),
            registered_at: Utc::now().to_rfc3339(),
            expires_at: Utc::now()
                .checked_add_signed(chrono::Duration::seconds(expires.into()))
                .unwrap_or_else(Utc::now)
                .to_rfc3339(),
            user_agent: req.user_agent.clone().unwrap_or_else(|| "".to_string()),
            expires: expires as u64,
        };

        let _ = Self::store_to_redis(&data).await;

        let contact_key = Self::contact_key(&user, &hashed_callid);
        let all_contacts_key = Self::all_contacts_key(proxy_host);

        if expires > 0 {
            let _ = REDIS.sadd(&all_contacts_key, &contact_key).await;
        } else {
            let _ = REDIS.srem(&all_contacts_key, &contact_key).await;
        }

        Some(data)
    }

    pub async fn update_usrloc(data: &UsrlocData) -> Result<()> {
        let contact_key = Self::contact_key(&data.user, &data.location_id);
        REDIS
            .hsetex(&contact_key, PROXY_HOST, &data.proxy_host)
            .await?;
        Ok(())
    }

    pub async fn store_to_redis(data: &UsrlocData) -> Result<()> {
        let contact_key = Self::contact_key(&data.user, &data.location_id);
        let location_key = Self::location_key(&data.user);

        if data.expires > 0 {
            REDIS
                .hmset(
                    &contact_key,
                    vec![
                        (USER, &data.user),
                        (CONTACT, &data.contact),
                        (RECEIVED, &data.received),
                        (REGISTER_AT, &data.registered_at),
                        (EXPIRES_AT, &data.expires_at),
                        (USER_AGENT, &data.user_agent),
                        (CALL_ID, &data.callid),
                        (LOCATION_ID, &data.location_id),
                        (PROXY_HOST, &data.proxy_host),
                    ],
                )
                .await?;
            let _ = REDIS.expire(&contact_key, data.expires).await;
            let _ = REDIS.lrem(&location_key, 0, &data.location_id).await;
            let _ = REDIS.lpush(&location_key, &data.location_id).await;
            // only stores the recently 10 registraions
            let _ = REDIS.ltrim(&location_key, 0, 9).await;
        } else {
            let _ = REDIS.del(&contact_key).await;
            let _ = REDIS.lrem(&location_key, 0, &data.location_id).await;
        }
        Ok(())
    }

    pub async fn lookup_one(
        user: &User,
        hashed_callid: &str,
        location_key: &str,
        seen: &mut HashSet<(String, u16, TransportType)>,
    ) -> Option<Location> {
        let contact_map_result: Result<HashMap<String, String>, _> = REDIS
            .hgetall(&Self::contact_key(&user.name.to_lowercase(), hashed_callid))
            .await;
        let valid_result = match &contact_map_result {
            Ok(map_result) => !map_result.is_empty(),
            Err(_) => false,
        };
        if !valid_result {
            let _ = REDIS.lrem(location_key, 0, hashed_callid).await;
            return None;
        }

        let contact_map = contact_map_result.ok()?;
        let req_uri = Address::from_str(contact_map.get(CONTACT)?).ok()?.uri;
        let dst_uri = Uri::from_str(contact_map.get(RECEIVED)?).ok()?;
        let user_agent = contact_map.get(USER_AGENT).cloned();
        let proxy_host = contact_map.get(PROXY_HOST)?;
        let dnd = contact_map
            .get("dnd")
            .map(|dnd| dnd == "yes")
            .unwrap_or(false);
        let dnd_allow_internal = contact_map
            .get("dnd_allow_internal")
            .map(|dnd_allow_internal| dnd_allow_internal == "yes")
            .unwrap_or(false);

        let received = (
            dst_uri.host.clone(),
            dst_uri.get_port(),
            dst_uri.transport.clone(),
        );
        if seen.contains(&received) {
            return None;
        }
        seen.insert(received);

        Some(Location {
            req_uri,
            dst_uri: dst_uri.clone(),
            user: user.name.clone(),
            from_host: user
                .domain
                .clone()
                .unwrap_or_else(|| "talk.yay.com".to_string()),
            to_host: user
                .domain
                .clone()
                .unwrap_or_else(|| "talk.yay.com".to_string()),
            proxy_host: proxy_host.to_string(),
            display_name: user.display_name.clone(),
            intra: false,
            inter_tenant: None,
            encryption: if dst_uri.transport == TransportType::Tls {
                true
            } else {
                user.encryption.unwrap_or(false)
            },
            route: None,
            device: None,
            device_token: None,
            user_agent,
            dnd,
            dnd_allow_internal,
            provider: None,
            is_registration: Some(true),
            tenant_id: Some(user.tenant_id.clone()),
            teams_refer_cx: None,
            ziron_tag: None,
            diversion: None,
            pai: None,
            global_dnd: false,
            global_forward: None,
            room_auth_token: None,
        })
    }

    pub async fn lookup(user: &User) -> Result<Vec<Location>, Error> {
        let username = &user.name.to_lowercase();
        let location_key = &Self::location_key(username);
        let hashed_callids: Vec<String> = REDIS.lrange(location_key, 0, -1).await?;

        let mut seen = HashSet::new();
        let mut locations = Vec::new();
        for hashed_callid in hashed_callids.iter() {
            if let Some(location) =
                Self::lookup_one(user, hashed_callid, location_key, &mut seen).await
            {
                if location.dst_uri.transport != TransportType::Wss
                    && location.dst_uri.transport != TransportType::Ws
                    && locations.len() > 3
                {
                    continue;
                }

                locations.push(location);
            }
        }

        Ok(locations)
    }

    pub async fn check_usrloc(msg: &Message) -> bool {
        let expires = match msg.expires {
            Some(e) => e,
            None => match &msg.contact {
                Some(c) => c.expires.unwrap_or(0),
                None => 0,
            },
        };
        let Some(authorization) = msg.authorization.as_ref() else {
            return false;
        };
        let user = authorization.username.to_lowercase();
        let hashed_callid = sha256(&msg.callid);

        let contact_key = Self::contact_key(&user, &hashed_callid);

        if expires == 0 {
            if !REDIS.exists(&contact_key).await.unwrap_or(false) {
                // if we don't have the contact key,
                // there's nothing to de-register
                let _ = TM
                    .reply(msg, 404, "Not Found".to_string(), None, None)
                    .await;
                return false;
            }
            return true;
        }
        let ttl = REDIS.ttl(&contact_key).await.unwrap_or(0);
        if ttl + 10 > expires as i64 {
            let _ = TM
                .reply(msg, 423, "Interval Too Brief".to_string(), None, None)
                .await;
            // only accept re-register every 10 seconds
            return false;
        }

        true
    }

    pub fn location_key(user: &str) -> String {
        format!("nebula:location:{}", user)
    }

    pub fn contact_key(user: &str, hashed_callid: &str) -> String {
        format!("nebula:location:{}:{}", user, hashed_callid)
    }

    pub fn all_contacts_key(porxy_host: &str) -> String {
        format!("nebula:all_contacts:{porxy_host}")
    }

    pub fn response(
        &self,
        req: &Message,
        code: i32,
        status: &str,
        expires: i32,
    ) -> Message {
        let mut resp = Message::default();

        resp.code = Some(code);
        resp.status = Some(status.to_string());
        resp.via = req.via.clone();
        resp.to = req.to.clone();
        resp.from = req.from.clone();
        resp.callid = req.callid.clone();
        resp.contact = req.contact.clone();
        resp.contact.as_mut().map(|c| c.expires = Some(expires));
        resp.cseq = req.cseq.clone();

        resp
    }
}
