use super::server::SWITCHBOARD_SERVICE;
use crate::{
    channel::{
        redirect_inbound, user_locations, Channel, ChannelBridgeResult,
        ChannelHangupDirection,
    },
    room::Room,
    switchboard::{format_national_number, get_phone_number},
};
use anyhow::{anyhow, Result};
use bigdecimal::BigDecimal;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use nebula_db::{
    message::{
        CallConfirmation, ChannelEvent, Endpoint, Response, TransferPeer,
        UserAgentType,
    },
    models::{self, CdrItemChange, Group, Number, Trunk, User},
};
use nebula_redis::{DistributedMutex, REDIS};
use phonenumber::PhoneNumber;
use rand::seq::SliceRandom;
use sip::{
    message::{Address, CallerId, HangupReason, InterTenantInfo, Location, Uri},
    transport::TransportType,
};
use std::str::FromStr;
use std::time::Duration;
use tracing::{info, warn};
use yaypi::api::YaypiClient;

#[derive(Clone, Debug)]
pub struct ExternalNumber {
    pub tenant_id: String,
    pub number: PhoneNumber,
    pub provider: String,
    pub backup_provider: Option<String>,
    pub caller_id: Option<CallerId>,
    pub backup_caller_id: Option<CallerId>,
    pub operator_code: Option<(String, bool, bool)>,
}

#[derive(Clone, Debug)]
pub enum BridgeExtension {
    User(User),
    Group(Group),
    Trunk(Trunk, Option<String>, Option<Address>, Option<Address>),
    ExternalNumber(ExternalNumber, Option<CallConfirmation>),
    Intra(Number, Option<CallConfirmation>),
    InterTenant(InterTenantInfo),
}

impl BridgeExtension {
    pub fn id(&self) -> String {
        match self {
            BridgeExtension::User(u) => format!("user:{}", u.uuid),
            BridgeExtension::Group(g) => format!("group:{}", g.uuid),
            BridgeExtension::Trunk(t, _, _, _) => format!("trunk:{}", t.uuid),
            BridgeExtension::ExternalNumber(n, _) => format!(
                "external_number:{}",
                n.number.format().mode(phonenumber::Mode::E164).to_string()
            ),
            BridgeExtension::Intra(number, _) => {
                format!("internal_number:{}", number.e164)
            }
            BridgeExtension::InterTenant(InterTenantInfo {
                dst_tenant_id,
                exten,
                ..
            }) => {
                format!("inter_tenant:{}:{}", dst_tenant_id, exten)
            }
        }
    }

    pub fn user(&self) -> Option<&User> {
        match self {
            BridgeExtension::User(u) => Some(u),
            _ => None,
        }
    }

    pub fn has_user(&self, user_name: &str) -> bool {
        match self {
            BridgeExtension::User(u) => {
                u.name.to_lowercase() == user_name.to_lowercase()
            }
            BridgeExtension::Group(g) => {
                for user in &g.members {
                    if user.name.to_lowercase() == user_name.to_lowercase() {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub fn call_confirmation(&self) -> Option<CallConfirmation> {
        match self {
            BridgeExtension::ExternalNumber(_, confirmation) => confirmation.clone(),
            BridgeExtension::Intra(_, confirmation) => confirmation.clone(),
            _ => None,
        }
    }

    #[async_recursion::async_recursion]
    pub async fn user_to_locations(
        user: &User,
        internal_call: bool,
        group: Option<String>,
        include_login_as: bool,
    ) -> Vec<(
        Endpoint,
        Location,
        Option<Location>,
        Option<CallerId>,
        Option<CallerId>,
    )> {
        // read user again to get latest
        let refresh_user = SWITCHBOARD_SERVICE
            .db
            .get_user(&user.tenant_id, &user.uuid)
            .await;
        let user = refresh_user.as_ref().unwrap_or(user);
        if user.global_dnd.unwrap_or(false) {
            let uri = Uri {
                user: Some(user.name.clone()),
                host: "talk.yay.com".to_string(),
                ..Default::default()
            };
            return vec![(
                Endpoint::User {
                    user: user.clone(),
                    group: group.clone(),
                    user_agent: None,
                    user_agent_type: None,
                },
                Location {
                    user: user.name.clone(),
                    req_uri: uri.clone(),
                    dst_uri: uri,
                    proxy_host: SWITCHBOARD_SERVICE.config.proxy_host.clone(),
                    from_host: user
                        .domain
                        .clone()
                        .unwrap_or_else(|| "talk.yay.com".to_string()),
                    to_host: user
                        .domain
                        .clone()
                        .unwrap_or_else(|| "talk.yay.com".to_string()),
                    display_name: user.display_name.clone(),
                    route: None,
                    intra: false,
                    inter_tenant: None,
                    encryption: false,
                    device: None,
                    device_token: None,
                    dnd: false,
                    dnd_allow_internal: false,
                    provider: None,
                    is_registration: None,
                    user_agent: None,
                    tenant_id: Some(user.tenant_id.clone()),
                    teams_refer_cx: None,
                    ziron_tag: None,
                    diversion: None,
                    pai: None,
                    global_dnd: true,
                    global_forward: None,
                    room_auth_token: None,
                },
                None,
                None,
                None,
            )];
        }

        if let Some(global_forward) = user.global_forward.as_ref() {
            let global_forward = global_forward.trim();
            if !global_forward.is_empty() {
                let uri = Uri {
                    user: Some(user.name.clone()),
                    host: "talk.yay.com".to_string(),
                    ..Default::default()
                };
                return vec![(
                    Endpoint::User {
                        user: user.clone(),
                        group: group.clone(),
                        user_agent: None,
                        user_agent_type: None,
                    },
                    Location {
                        user: user.name.clone(),
                        req_uri: uri.clone(),
                        dst_uri: uri,
                        proxy_host: SWITCHBOARD_SERVICE.config.proxy_host.clone(),
                        from_host: user
                            .domain
                            .clone()
                            .unwrap_or_else(|| "talk.yay.com".to_string()),
                        to_host: user
                            .domain
                            .clone()
                            .unwrap_or_else(|| "talk.yay.com".to_string()),
                        display_name: user.display_name.clone(),
                        route: None,
                        intra: false,
                        inter_tenant: None,
                        encryption: false,
                        device: None,
                        device_token: None,
                        dnd: false,
                        dnd_allow_internal: false,
                        provider: None,
                        is_registration: None,
                        user_agent: None,
                        tenant_id: Some(user.tenant_id.clone()),
                        teams_refer_cx: None,
                        ziron_tag: None,
                        diversion: None,
                        pai: None,
                        global_dnd: false,
                        global_forward: Some(global_forward.to_string()),
                        room_auth_token: None,
                    },
                    None,
                    None,
                    None,
                )];
            }
        }

        let mut locations: Vec<(
            Endpoint,
            Location,
            Option<Location>,
            Option<CallerId>,
            Option<CallerId>,
        )> = user_locations(user, internal_call)
            .await
            .into_iter()
            .map(|l| {
                let user_agent_type = if l.dst_uri.transport == TransportType::Ws
                    || l.dst_uri.transport == TransportType::Wss
                {
                    Some(UserAgentType::DesktopApp)
                } else if l.device.is_some() && l.device_token.is_some() {
                    Some(UserAgentType::MobileApp)
                } else if l.dst_uri.host == "sip.pstnhub.microsoft.com" {
                    Some(UserAgentType::MsTeams)
                } else {
                    None
                };

                (
                    Endpoint::User {
                        user: user.clone(),
                        group: group.clone(),
                        user_agent: None,
                        user_agent_type,
                    },
                    l,
                    None,
                    None,
                    None,
                )
            })
            .collect();
        if include_login_as {
            if let Some(users) =
                SWITCHBOARD_SERVICE.db.get_login_users(&user.uuid).await
            {
                for user_uuid in users {
                    if user_uuid != user.uuid {
                        if let Some(user) = SWITCHBOARD_SERVICE
                            .db
                            .get_user(&user.tenant_id, &user_uuid)
                            .await
                        {
                            locations.extend(
                                Self::user_to_locations(
                                    &user,
                                    internal_call,
                                    group.clone(),
                                    false,
                                )
                                .await
                                .into_iter(),
                            )
                        }
                    }
                }
            }
        }
        locations
    }

    /// Get the locations of the xtension to bridge
    /// e.g. for a sip user, that would be the registration, app etc
    /// also, for a sip user, if disable_call_waiting is enabled,
    /// we send 0 locations if the user is already on a call
    pub async fn locations(
        &self,
        internal_call: bool,
        to_exten: String,
        include_login_as: bool,
    ) -> Option<
        Vec<(
            Endpoint,
            Location,
            Option<Location>,
            Option<CallerId>,
            Option<CallerId>,
        )>,
    > {
        let locations = match self {
            BridgeExtension::User(u) => {
                Self::user_to_locations(u, internal_call, None, include_login_as)
                    .await
            }
            BridgeExtension::Group(g) => {
                let mut locations = Vec::new();
                for user in g.members.iter() {
                    locations.push(
                        Self::user_to_locations(
                            user,
                            internal_call,
                            Some(g.name.clone()),
                            include_login_as,
                        )
                        .await,
                    );
                }
                locations.concat()
            }
            BridgeExtension::Trunk(t, ziron_tag, diversion, pai) => {
                let user = format_trunk_number(&t.user, &to_exten)?;
                let req_uri = Uri {
                    scheme: "sip".to_string(),
                    user: Some(user.clone()),
                    host: t.domain.clone(),
                    port: t.port.map(|p| p as u16),
                    transport: TransportType::from_str(&t.transport.to_lowercase())
                        .ok()?,
                    ..Default::default()
                };
                let dst_uri = Uri {
                    scheme: "sip".to_string(),
                    user: Some(user.clone()),
                    host: t.domain.clone(),
                    port: t.port.map(|p| p as u16),
                    transport: TransportType::from_str(&t.transport.to_lowercase())
                        .ok()?,
                    ..Default::default()
                };
                let diversion = if t.show_diversion.unwrap_or(false) {
                    let mut diversion = diversion.to_owned();
                    if let Some(d) = diversion.as_mut() {
                        d.uri.host = "34.105.209.181".to_string();
                    }
                    diversion
                } else {
                    None
                };
                let pai = if t.pass_pai.unwrap_or(false) {
                    let mut pai = pai.to_owned();
                    if let Some(d) = pai.as_mut() {
                        d.uri.host = "34.105.209.181".to_string();
                    }
                    pai
                } else {
                    None
                };
                let user_agent = if t.user_agent.unwrap_or(false) {
                    Some("Nebula".to_string())
                } else {
                    None
                };
                vec![(
                    Endpoint::Trunk {
                        trunk: t.clone(),
                        number: user.clone(),
                        extension: "".to_string(),
                        anonymous: false,
                        ziron_tag: ziron_tag.clone(),
                    },
                    Location {
                        user,
                        from_host: "talk.yay.com".to_string(),
                        to_host: t.domain.clone(),
                        proxy_host: SWITCHBOARD_SERVICE.config.proxy_host.clone(),
                        display_name: None,
                        req_uri,
                        dst_uri,
                        intra: false,
                        inter_tenant: None,
                        encryption: false,
                        route: None,
                        device: None,
                        device_token: None,
                        dnd: false,
                        dnd_allow_internal: false,
                        user_agent,
                        provider: Some(true),
                        is_registration: None,
                        tenant_id: Some(t.tenant_id.clone()),
                        teams_refer_cx: None,
                        ziron_tag: ziron_tag.clone(),
                        diversion,
                        pai,
                        global_dnd: false,
                        global_forward: None,
                        room_auth_token: None,
                    },
                    None,
                    None,
                    None,
                )]
            }
            BridgeExtension::ExternalNumber(n, _) => {
                let location = external_number_location(
                    &n.number,
                    &n.provider,
                    &n.operator_code,
                )?;
                let backup_location = n.backup_provider.as_ref().and_then(|b| {
                    external_number_location(&n.number, b, &n.operator_code)
                });
                let e164 =
                    n.number.format().mode(phonenumber::Mode::E164).to_string();
                vec![(
                    Endpoint::Provider {
                        provider: models::Provider {
                            id: 0,
                            name: n.provider.clone(),
                            host: Some(n.provider.clone()),
                            ..Default::default()
                        },
                        number: e164,
                        anonymous: false,
                        tenant_id: Some(n.tenant_id.clone()),
                        diversion: None,
                        pai: None,
                    },
                    location,
                    backup_location,
                    n.caller_id.clone(),
                    n.backup_caller_id.clone(),
                )]
            }
            BridgeExtension::Intra(n, _) => {
                let e164 = format!("+{}", n.e164);
                let uri = Uri {
                    scheme: "sip".to_string(),
                    user: Some(e164.clone()),
                    host: "intra".to_string(),
                    ..Default::default()
                };
                vec![(
                    Endpoint::Provider {
                        provider: models::Provider {
                            id: 0,
                            name: "intra".to_string(),
                            host: Some("intra".to_string()),
                            ..Default::default()
                        },
                        number: e164.clone(),
                        anonymous: false,
                        tenant_id: None,
                        diversion: None,
                        pai: None,
                    },
                    Location {
                        user: e164,
                        from_host: "intra".to_string(),
                        to_host: "intra".to_string(),
                        proxy_host: SWITCHBOARD_SERVICE.config.proxy_host.clone(),
                        display_name: None,
                        req_uri: uri.clone(),
                        dst_uri: uri,
                        intra: true,
                        inter_tenant: None,
                        encryption: false,
                        route: None,
                        device: None,
                        device_token: None,
                        dnd: false,
                        dnd_allow_internal: false,
                        provider: None,
                        is_registration: None,
                        user_agent: None,
                        tenant_id: n.tenant_id.clone(),
                        teams_refer_cx: None,
                        ziron_tag: None,
                        diversion: None,
                        pai: None,
                        global_dnd: false,
                        global_forward: None,
                        room_auth_token: None,
                    },
                    None,
                    None,
                    None,
                )]
            }
            BridgeExtension::InterTenant(InterTenantInfo {
                src_tenant_id,
                dst_tenant_id,
                exten,
                from_exten,
                from_display_name,
                extern_call_flow,
            }) => {
                let uri = Uri {
                    scheme: "sip".to_string(),
                    user: Some(exten.to_string()),
                    host: "inter_tenant".to_string(),
                    ..Default::default()
                };
                vec![(
                    Endpoint::Provider {
                        provider: models::Provider {
                            id: 0,
                            name: "intra".to_string(),
                            host: Some("intra".to_string()),
                            ..Default::default()
                        },
                        number: exten.to_string(),
                        anonymous: false,
                        tenant_id: None,
                        diversion: None,
                        pai: None,
                    },
                    Location {
                        user: exten.to_string(),
                        from_host: "intra".to_string(),
                        to_host: "intra".to_string(),
                        proxy_host: SWITCHBOARD_SERVICE.config.proxy_host.clone(),
                        display_name: None,
                        req_uri: uri.clone(),
                        dst_uri: uri,
                        intra: true,
                        inter_tenant: Some(InterTenantInfo {
                            exten: exten.to_string(),
                            src_tenant_id: src_tenant_id.to_string(),
                            dst_tenant_id: dst_tenant_id.to_string(),
                            from_exten: from_exten.to_string(),
                            from_display_name: from_display_name.to_string(),
                            extern_call_flow: *extern_call_flow,
                        }),
                        encryption: false,
                        route: None,
                        device: None,
                        device_token: None,
                        dnd: false,
                        dnd_allow_internal: false,
                        provider: None,
                        user_agent: None,
                        is_registration: None,
                        tenant_id: Some(dst_tenant_id.to_string()),
                        teams_refer_cx: None,
                        ziron_tag: None,
                        diversion: None,
                        pai: None,
                        global_dnd: false,
                        global_forward: None,
                        room_auth_token: None,
                    },
                    None,
                    None,
                    None,
                )]
            }
        };
        Some(locations)
    }
}

#[derive(Clone)]
pub struct Bridge {
    pub id: String,
}

impl Bridge {
    pub fn new(id: String) -> Bridge {
        Bridge { id }
    }

    pub async fn set_announcement(&self, announcement: &str) -> Result<()> {
        REDIS
            .hset(
                &format!("nebula:bridge:{}", &self.id),
                "announcement",
                announcement,
            )
            .await?;
        Ok(())
    }

    pub async fn get_announcement(&self) -> String {
        REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "announcement")
            .await
            .unwrap_or("".to_string())
    }

    pub async fn set_ringtone(&self, ringtone: &str) -> Result<()> {
        REDIS
            .hset(&format!("nebula:bridge:{}", &self.id), "ringtone", ringtone)
            .await?;
        Ok(())
    }

    pub async fn get_ringtone(&self) -> String {
        REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "ringtone")
            .await
            .unwrap_or("".to_string())
    }

    pub async fn set_tenant_id(&self, tenant_id: &str) -> Result<()> {
        REDIS
            .hset(
                &format!("nebula:bridge:{}", &self.id),
                "tenant_id",
                tenant_id,
            )
            .await?;
        Ok(())
    }

    pub async fn set_inbound(&self, id: &str) -> Result<()> {
        REDIS
            .hset(&format!("nebula:bridge:{}", &self.id), "inbound", id)
            .await?;
        Ok(())
    }

    pub async fn get_inbound(&self) -> Result<Channel> {
        let id: String = REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "inbound")
            .await?;
        Ok(Channel::new(id))
    }

    pub async fn cancel(&self) -> Result<()> {
        self.hangup(ChannelBridgeResult::Cancelled).await?;
        Ok(())
    }

    fn mutex(&self) -> DistributedMutex {
        DistributedMutex::new(format!("nebula:bridge:{}:lock", self.id))
    }

    pub async fn handle_timeout(
        &self,
        deadline: tokio::time::Instant,
        send_completed_elsewhere: bool,
    ) -> Result<()> {
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}", &self.id), 600)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:channels", &self.id), 600)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:hangup_channels", &self.id), 600)
            .await;

        let mut stream_key = "0".to_string();
        loop {
            tokio::select! {
                (key_id, event_name, _event_value) = self.next_event(&stream_key) => {
                    match ChannelEvent::from_str(&event_name)? {
                        ChannelEvent::BridgeAnswer | ChannelEvent::BridgeHangup => {
                            return Ok(());
                        }
                        _ => (),
                    };
                    stream_key = key_id;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    self.hangup(
                       if send_completed_elsewhere {
                            ChannelBridgeResult::NoanswerWithCompletedElsewhere
                        } else {
                            ChannelBridgeResult::Noanswer
                        }
                    ).await?;
                    return Ok(());
                }
            }
        }
    }

    pub async fn is_hangup(&self) -> Result<bool> {
        if !REDIS
            .exists(&format!("nebula:bridge:{}", &self.id))
            .await
            .unwrap_or(false)
        {
            return Ok(true);
        }
        Ok(REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "hangup")
            .await
            .unwrap_or("".to_string())
            == "yes")
    }

    async fn talking_end(&self) -> Result<()> {
        let inbound = self.get_inbound().await?;

        tokio::spawn(async move {
            let cdr = inbound.get_cdr().await.ok_or(anyhow!("no cdr"))?;
            let talking_start =
                cdr.talking_start.ok_or(anyhow!("no talking start"))?;
            let talking_end = Utc::now();
            let talking_duration = talking_end
                .signed_duration_since(talking_start)
                .num_seconds();
            inbound
                .update_cdr(CdrItemChange {
                    talking_end: Some(talking_end),
                    talking_duration: Some(talking_duration),
                    ..Default::default()
                })
                .await?;
            Ok::<(), anyhow::Error>(())
        });

        Ok(())
    }

    pub async fn wait(&self) -> Result<ChannelBridgeResult> {
        if self.channels().await.unwrap_or_default().is_empty() {
            let local_bridge = self.clone();
            tokio::spawn(async move {
                local_bridge.hangup(ChannelBridgeResult::Unavailable).await
            });
        }
        let mut stream_key = "0".to_string();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(86400)) => {
                    return Err(anyhow!("never received bridge hangup"));
                }
                (key_id, event_name, event_value) = self.next_event(&stream_key) => {
                    if let ChannelEvent::BridgeHangup = ChannelEvent::from_str(&event_name)?
                    {
                        let _ = REDIS
                            .xaddex::<String>(
                                &format!("nebula:bridge:{}:stream", &self.id),
                                &ChannelEvent::BridgeHangupAck.to_string(),
                                "ack",
                                60,
                            )
                            .await;
                        return Ok(ChannelBridgeResult::from_str(&event_value)?);
                    };
                    stream_key = key_id;
                }
            }
        }
    }

    pub async fn next_event(&self, stream_key: &str) -> (String, String, String) {
        let stream = format!("nebula:bridge:{}:stream", &self.id);
        loop {
            let result = REDIS
                .xread_next_entry_timeout(&stream, stream_key, 1000)
                .await;
            if let Ok(result) = result {
                return result;
            }
        }
    }

    pub async fn endpoint_hangup_result(
        &self,
        endpoint: &Endpoint,
    ) -> Result<ChannelBridgeResult> {
        for c in self.channels().await? {
            if endpoint.id() == c.get_call_context().await?.endpoint.id() {
                return Err(anyhow!("user is not hangup"));
            }
        }

        let results: Vec<ChannelBridgeResult> =
            stream::iter(self.hangup_channels().await?)
                .filter_map(|channel| async move {
                    if endpoint.id()
                        == channel.get_call_context().await.ok()?.endpoint.id()
                    {
                        if channel.is_answered().await {
                            Some(ChannelBridgeResult::Ok)
                        } else {
                            if let Ok(hangup) = channel.get_hangup().await {
                                match hangup {
                                    ChannelHangupDirection::Server => {
                                        Some(ChannelBridgeResult::Noanswer)
                                    }
                                    ChannelHangupDirection::Client => {
                                        Some(ChannelBridgeResult::Busy)
                                    }
                                }
                            } else {
                                None
                            }
                        }
                    } else {
                        None
                    }
                })
                .collect()
                .await;
        if results.len() == 0 {
            Ok(ChannelBridgeResult::Noanswer)
        } else if results.iter().any(|r| r == &ChannelBridgeResult::Ok) {
            Ok(ChannelBridgeResult::Ok)
        } else if results.iter().all(|r| r == &ChannelBridgeResult::Busy) {
            Ok(ChannelBridgeResult::Busy)
        } else {
            Ok(ChannelBridgeResult::Noanswer)
        }
    }

    pub async fn add_channel(&self, channel: &str) -> Result<()> {
        REDIS
            .sadd(&format!("nebula:bridge:{}:channels", &self.id), channel)
            .await?;
        REDIS
            .hset(
                &format!("nebula:channel:{}", channel),
                "bridge_id",
                &self.id,
            )
            .await?;
        Ok(())
    }

    pub async fn channels(&self) -> Result<Vec<Channel>> {
        REDIS
            .smembers::<Vec<String>>(&format!("nebula:bridge:{}:channels", &self.id))
            .await
            .map(|members| {
                members
                    .iter()
                    .map(|m| Channel::new(m.to_string()))
                    .collect()
            })
    }

    pub async fn hangup_channels(&self) -> Result<Vec<Channel>> {
        REDIS
            .smembers::<Vec<String>>(&format!(
                "nebula:bridge:{}:hangup_channels",
                &self.id
            ))
            .await
            .map(|members| {
                members
                    .iter()
                    .map(|m| Channel::new(m.to_string()))
                    .collect()
            })
    }

    pub async fn answered(&self) -> bool {
        self.answer_channel().await.unwrap_or("".to_string()) != ""
    }

    pub async fn handle_response(
        &self,
        src_channel: Channel,
        resp: &Response,
    ) -> Result<()> {
        self.mutex().lock().await;

        if self.answered().await {
            return Ok(());
        }
        if self.is_hangup().await.unwrap_or(false) {
            return Ok(());
        }
        match resp.code {
            180 | 183 => {
                let ringing = self.is_ringing().await.unwrap_or(0);
                if resp.code <= ringing {
                    return Ok(());
                }
                REDIS
                    .hset(
                        &format!("nebula:bridge:{}", &self.id),
                        "ringing",
                        &resp.code.to_string(),
                    )
                    .await?;
                let inbound = self.get_inbound().await?;
                let ringtone = self.get_ringtone().await;
                if ringtone != "" {
                    let _ = inbound.ring(ringtone).await;
                    return Ok(());
                }
                if resp.code == 183 {
                    if self.num_channels().await? == 1 {
                        let channels = self.channels().await?;
                        if channels.len() == 1 {
                            if let Ok(context) = channels[0].get_call_context().await
                            {
                                if context.endpoint.provider().is_some() {
                                    let _ =
                                        inbound.session_progress(&src_channel).await;
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                let _ = inbound.ring(ringtone).await;
            }
            _ => (),
        }
        Ok(())
    }

    pub async fn num_channels(&self) -> Result<usize> {
        let channels = REDIS
            .scard(&format!("nebula:bridge:{}:channels", &self.id))
            .await
            .unwrap_or(0);
        let hangup_channels = REDIS
            .scard(&format!("nebula:bridge:{}:hangup_channels", &self.id))
            .await
            .unwrap_or(0);
        Ok(channels + hangup_channels)
    }

    pub async fn answer(&self, outbound_channel: &Channel) -> Result<()> {
        info!(
            bridge = self.id,
            channel = outbound_channel.id,
            "bridge got answer from {}",
            outbound_channel.id
        );
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}", &self.id), 86400)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:channels", &self.id), 86400)
            .await;
        let _ = REDIS
            .expire(
                &format!("nebula:bridge:{}:hangup_channels", &self.id),
                86400,
            )
            .await;

        let outbound_context = outbound_channel.get_call_context().await?;

        let announcement = self.get_announcement().await;
        if let Some(confirmation) = outbound_context.call_confirmation.as_ref() {
            let timeout = if confirmation.timeout == 0 {
                3
            } else {
                confirmation.timeout
            };
            let sound = if confirmation.sound == "" {
                if confirmation.key == "" {
                    "tts://Press any key to accept this call".to_string()
                } else {
                    format!("tts://Press {} to accept this call", confirmation.key)
                }
            } else {
                confirmation.sound.to_string()
            };

            let sounds = if announcement.is_empty() {
                vec![sound]
            } else {
                vec![announcement.clone(), sound]
            };

            let pressed = outbound_channel
                .play_sound_and_get_dtmf(sounds, timeout * 1000, 5000, 1)
                .await
                .join("");
            if pressed == "" {
                outbound_channel.hangup(None).await?;
                return Ok(());
            }
            if confirmation.key != "" && confirmation.key != pressed {
                outbound_channel.hangup(None).await?;
                return Ok(());
            }
        }

        self.mutex().lock().await;

        if self.is_hangup().await.unwrap_or(false) {
            return Err(anyhow!("bridge already hung up"));
        }
        if self.answered().await {
            return Err(anyhow!("bridge already answered"));
        }

        let tenant_id = REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "tenant_id")
            .await
            .unwrap_or("".to_string());
        if tenant_id != "" {
            info!(
                bridge = self.id,
                "remove {} from {}'s incoming_bridge because it's answered",
                &self.id,
                tenant_id
            );
            let _ = REDIS
                .lrem(
                    &format!("nebula:tenant:{}:incoming_bridge", tenant_id),
                    0,
                    &self.id,
                )
                .await;
        }

        let _ = REDIS
            .hset(
                &format!("nebula:bridge:{}", &self.id),
                "answer_channel",
                &outbound_channel.id,
            )
            .await;

        let stream = format!("nebula:bridge:{}:stream", &self.id);
        let channels: Vec<String> = REDIS
            .smembers(&format!("nebula:bridge:{}:channels", &self.id))
            .await?;
        for c in channels {
            if c != outbound_channel.id {
                REDIS
                    .srem(&format!("nebula:bridge:{}:channels", &self.id), &c)
                    .await?;
                REDIS
                    .sadd(&format!("nebula:bridge:{}:hangup_channels", &self.id), &c)
                    .await?;
                REDIS
                    .xaddex(&stream, &ChannelEvent::Hangup.to_string(), &c, 60)
                    .await?;
                info!(
                    bridge = self.id,
                    channel = c,
                    "bridge hangup channel because bridge was answered"
                );
                tokio::spawn(async move {
                    let _ = Channel::new(c.to_string())
                        .hangup(Some(HangupReason::CompletedElsewhere))
                        .await;
                });
            }
        }
        REDIS
            .xaddex(
                &stream,
                &ChannelEvent::BridgeAnswer.to_string(),
                &outbound_channel.id,
                60,
            )
            .await?;

        // If there's call confirmation, the annoucement has already played there
        if !announcement.is_empty() && outbound_context.call_confirmation.is_none() {
            let _ = outbound_channel
                .play_sound(None, vec![announcement], true)
                .await;
        }

        if let Some((destination, proxy_host)) = outbound_context.click_to_dial {
            let outbound_channel = outbound_channel.clone();
            let endpoint = outbound_context.endpoint.clone();
            tokio::spawn(async move {
                println!("now switch click to dial {}", outbound_channel.id);

                let _ = SWITCHBOARD_SERVICE
                    .switchboard_rpc
                    .new_inbound_channel(
                        &SWITCHBOARD_SERVICE.config.switchboard_host,
                        outbound_channel.id.clone(),
                        &endpoint,
                        &proxy_host,
                        &destination,
                        "",
                        true,
                        None,
                        None,
                        None,
                        Vec::new(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
            });
            return Ok(());
        }

        if let Some(callflow) = outbound_context.callflow {
            let outbound_channel = outbound_channel.clone();
            let endpoint = outbound_context.endpoint.clone();

            let local_outbound_channel = outbound_channel.clone();
            let local_endpoint = endpoint.clone();

            println!("outbound now calling to inbound {}", outbound_channel.id);
            let outbound_channel_id = outbound_channel.id.clone();
            tokio::spawn(async move {
                let _ = SWITCHBOARD_SERVICE
                    .switchboard_rpc
                    .new_inbound_channel(
                        &SWITCHBOARD_SERVICE.config.switchboard_host,
                        outbound_channel_id,
                        &endpoint,
                        "",
                        "",
                        "",
                        true,
                        None,
                        Some(callflow),
                        None,
                        Vec::new(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
            });
            outbound_channel
                .update_cdr(CdrItemChange {
                    answer: Some(Utc::now()),
                    disposition: Some("answered".to_string()),
                    answer_type: Some("number".to_string()),
                    answer_exten: Some(outbound_context.endpoint.extension()),
                    ..Default::default()
                })
                .await?;
            return Ok(());
        }

        if !outbound_context.room.is_empty() {
            let addr = REDIS
                .hget(
                    &format!("nebula:channel:{}", &outbound_channel.id),
                    "api_addr",
                )
                .await
                .unwrap_or_else(|_| "".to_string());
            if addr.is_empty() {
                println!(
                    "handle outbound channle sip join room {}",
                    outbound_channel.id
                );
                let _ = outbound_channel.sip_join_room(&outbound_context.room).await;
            }
            return Ok(());
        }

        let inbound = self.get_inbound().await?;
        info!(
            channel = inbound.id,
            bridge = self.id,
            "bridge answer inbound channel"
        );
        inbound.answer().await?;

        let inbound_context = inbound.get_call_context().await?;

        if let Some(destination) = inbound_context.callback.clone() {
            let outbound_channel = outbound_channel.clone();
            let endpoint = outbound_context.endpoint.clone();

            let mut outbound_context = outbound_context.clone();
            // set the callback_peer so that we can cleanup when hangup
            outbound_context.callback_peer = Some(inbound.id);
            outbound_channel.set_call_context(&outbound_context).await?;

            tokio::spawn(async move {
                println!("now switch click to dial {}", outbound_channel.id);

                let _ = SWITCHBOARD_SERVICE
                    .switchboard_rpc
                    .new_inbound_channel(
                        &SWITCHBOARD_SERVICE.config.switchboard_host,
                        outbound_channel.id.clone(),
                        &endpoint,
                        &inbound_context.proxy_host,
                        &destination,
                        "",
                        true,
                        None,
                        None,
                        None,
                        Vec::new(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await;
            });
            return Ok(());
        }

        if let Some(cdr) = inbound_context.cdr.as_ref() {
            let local_inbound = inbound.clone();
            let now = Utc::now();
            tokio::spawn(async move {
                if let Some(cdr) = local_inbound.get_cdr().await {
                    if cdr.answer.is_none() {
                        let _ = local_inbound
                            .update_cdr(CdrItemChange {
                                answer: Some(now),
                                disposition: Some("answered".to_string()),
                                ..Default::default()
                            })
                            .await;
                    }
                }
            });
        }

        let inbound_audio_id = inbound.get_audio_id().await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .stop_ringtone(inbound_audio_id)
            .await?;

        if let Some(transfer_context) = inbound_context.transfer.as_ref() {
            match &transfer_context.peer {
                TransferPeer::Initiator { to_channel, .. } => {
                    if let Some(peer) =
                        Channel::new(to_channel.to_string()).get_peer().await
                    {
                        let _ = peer.stop_ringtone().await;
                        Self::bridge_media(&peer, outbound_channel).await?;
                        Self::bridge_media(outbound_channel, &peer).await?;
                    }
                }
                TransferPeer::Recipient { from_channel, .. } => {
                    if let Some(peer) =
                        Channel::new(from_channel.to_string()).get_peer().await
                    {
                        let _ = peer.stop_ringtone().await;
                        Self::bridge_media(&peer, outbound_channel).await?;
                        Self::bridge_media(outbound_channel, &peer).await?;
                    }
                }
            }
        } else {
            Self::bridge_media(&inbound, outbound_channel).await?;
            Self::bridge_media(outbound_channel, &inbound).await?;
        }

        let _ = inbound.check_call_monitoring().await;
        let _ = outbound_channel.check_call_monitoring().await;
        match outbound_context.endpoint {
            Endpoint::User { user, .. } => {
                inbound
                    .update_cdr(CdrItemChange {
                        answer_type: Some("sipuser".to_string()),
                        answer_user: Some(user.name.clone()),
                        answer_name: Some(
                            user.display_name.clone().unwrap_or("".to_string()),
                        ),
                        answer_uuid: Some(user.uuid.clone()),
                        answer_exten: Some(user.extension.unwrap_or(0).to_string()),
                        talking_start: Some(Utc::now()),
                        ..Default::default()
                    })
                    .await?;
            }
            Endpoint::Provider {
                number, provider, ..
            } => {
                let inbound = if let Some(redirect) =
                    redirect_inbound(&inbound_context).await
                {
                    let _ = inbound
                        .update_cdr(CdrItemChange {
                            answer_type: Some("number".to_string()),
                            answer_exten: Some(number.clone()),
                            answer_uuid: Some("".to_string()),
                            answer_user: Some("".to_string()),
                            answer_name: Some("".to_string()),
                            talking_start: Some(Utc::now()),
                            ..Default::default()
                        })
                        .await;
                    redirect
                } else {
                    inbound
                };
                inbound
                    .update_cdr(CdrItemChange {
                        answer_type: Some("number".to_string()),
                        answer_exten: Some(number.clone()),
                        answer_uuid: Some("".to_string()),
                        answer_user: Some("".to_string()),
                        answer_name: Some("".to_string()),
                        talking_start: Some(Utc::now()),
                        ..Default::default()
                    })
                    .await?;
                let _ = outbound_channel
                    .update_cdr(CdrItemChange {
                        answer: Some(Utc::now()),
                        disposition: Some("answered".to_string()),
                        answer_type: Some("number".to_string()),
                        answer_exten: Some(number.clone()),
                        answer_uuid: Some("".to_string()),
                        answer_user: Some("".to_string()),
                        answer_name: Some("".to_string()),
                        ..Default::default()
                    })
                    .await;
            }
            Endpoint::Trunk {
                trunk,
                number,
                extension,
                ..
            } => {
                inbound
                    .update_cdr(CdrItemChange {
                        answer_type: Some("trunk".to_string()),
                        answer_name: Some(trunk.name.clone()),
                        answer_user: Some(extension),
                        answer_uuid: Some(trunk.uuid.clone()),
                        answer_exten: Some(number.to_string()),
                        talking_start: Some(Utc::now()),
                        ..Default::default()
                    })
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn unbridge_media(
        src_channel: &Channel,
        dst_channel: &Channel,
    ) -> Result<()> {
        let src_audio_id = src_channel.get_audio_id().await?;
        let dst_audio_id = dst_channel.get_audio_id().await?;

        REDIS
            .srem(
                &format!("nebula:media_stream:{}:destinations", src_audio_id),
                &dst_audio_id,
            )
            .await?;
        REDIS
            .srem(
                &format!("nebula:media_stream:{}:sources", dst_audio_id),
                &src_audio_id,
            )
            .await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .update_media_destinations(src_audio_id)
            .await?;
        Ok(())
    }

    pub async fn bridge_media(
        src_channel: &Channel,
        dst_channel: &Channel,
    ) -> Result<()> {
        let src_audio_id = src_channel.get_audio_id().await?;
        let dst_audio_id = dst_channel.get_audio_id().await?;
        REDIS
            .sadd(
                &format!("nebula:media_stream:{}:destinations", src_audio_id),
                &dst_audio_id,
            )
            .await?;
        REDIS
            .sadd(
                &format!("nebula:media_stream:{}:sources", dst_audio_id),
                &src_audio_id,
            )
            .await?;
        SWITCHBOARD_SERVICE
            .media_rpc_client
            .update_media_destinations(src_audio_id)
            .await?;

        if let Ok(src_video_id) = src_channel.get_video_id().await {
            if let Ok(dst_video_id) = dst_channel.get_video_id().await {
                REDIS
                    .sadd(
                        &format!(
                            "nebula:media_stream:{}:destinations",
                            src_video_id
                        ),
                        &dst_video_id,
                    )
                    .await?;
                REDIS
                    .sadd(
                        &format!("nebula:media_stream:{}:sources", dst_video_id),
                        &src_video_id,
                    )
                    .await?;
                SWITCHBOARD_SERVICE
                    .media_rpc_client
                    .update_media_destinations(src_video_id)
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn is_ringing(&self) -> Result<i32> {
        REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "ringing")
            .await
    }

    pub async fn answer_channel(&self) -> Result<String> {
        Ok(REDIS
            .hget(&format!("nebula:bridge:{}", &self.id), "answer_channel")
            .await
            .unwrap_or_else(|_| "".to_string()))
    }

    async fn inbound_hangup(&self) -> Result<()> {
        if self.answered().await {
            self.hangup(ChannelBridgeResult::Ok).await?;
        } else {
            self.hangup(ChannelBridgeResult::Noanswer).await?;
        }
        Ok(())
    }

    pub async fn channel_hangup(&self, channel: &str) -> Result<()> {
        info!(
            bridge = self.id,
            channel = channel,
            "bridge got hangup from channel"
        );
        let inbound = self.get_inbound().await?;
        if inbound.id == channel {
            self.inbound_hangup().await?;
        } else {
            self.outbound_hangup(channel).await?;
        }

        Ok(())
    }

    async fn outbound_hangup(&self, outbound_channel: &str) -> Result<()> {
        REDIS
            .srem(
                &format!("nebula:bridge:{}:channels", &self.id),
                outbound_channel,
            )
            .await?;
        REDIS
            .sadd(
                &format!("nebula:bridge:{}:hangup_channels", &self.id),
                outbound_channel,
            )
            .await?;
        REDIS
            .xaddex(
                &format!("nebula:bridge:{}:stream", &self.id),
                &ChannelEvent::Hangup.to_string(),
                outbound_channel,
                60,
            )
            .await?;

        if self.answer_channel().await.unwrap_or_default() == outbound_channel {
            self.hangup(ChannelBridgeResult::Ok).await?;
            return Ok(());
        }

        let channels: Vec<String> = REDIS
            .smembers(&format!("nebula:bridge:{}:channels", &self.id))
            .await?;
        if channels.is_empty() {
            self.hangup(ChannelBridgeResult::Busy).await?;
            return Ok(());
        }
        Ok(())
    }

    pub async fn hangup(&self, result: ChannelBridgeResult) -> Result<()> {
        self.mutex().lock().await;

        if self.is_hangup().await.unwrap_or(false) {
            return Ok(());
        }

        info!(bridge = self.id, "bridge hangup {}", result);

        {
            let tenant_id = REDIS
                .hget(&format!("nebula:bridge:{}", &self.id), "tenant_id")
                .await
                .unwrap_or("".to_string());
            if tenant_id != "" {
                info!(
                    bridge = self.id,
                    "remove {} from {}'s incoming_bridge because it's hangup",
                    &self.id,
                    tenant_id
                );
                let _ = REDIS
                    .lrem(
                        &format!("nebula:tenant:{}:incoming_bridge", tenant_id),
                        0,
                        &self.id,
                    )
                    .await;
            }
        }

        if result != ChannelBridgeResult::Ok && self.answered().await {
            warn!(
                bridge = self.id,
                "bridge hangup on {} got non OK result but answered", self.id
            );
            return Ok(());
        }

        let _ = self.talking_end().await;

        let _ = REDIS
            .hset(&format!("nebula:bridge:{}", &self.id), "hangup", "yes")
            .await;
        let channels: Vec<String> = REDIS
            .smembers(&format!("nebula:bridge:{}:channels", &self.id))
            .await
            .unwrap_or_default();
        for c in channels {
            let _ = REDIS
                .srem(&format!("nebula:bridge:{}:channels", &self.id), &c)
                .await;
            let _ = REDIS
                .sadd(&format!("nebula:bridge:{}:hangup_channels", &self.id), &c)
                .await;
            let _ = REDIS
                .xaddex::<String>(
                    &format!("nebula:bridge:{}:stream", &self.id),
                    &ChannelEvent::Hangup.to_string(),
                    &c,
                    60,
                )
                .await;
            info!(
                bridge = self.id,
                channel = c,
                "bridge hangup channel because bridge was hangup {result:?}"
            );
            let result = result.clone();
            tokio::spawn(async move {
                let _ = Channel::new(c.to_string())
                    .hangup(
                        if result
                            == ChannelBridgeResult::NoanswerWithCompletedElsewhere
                        {
                            Some(HangupReason::CompletedElsewhere)
                        } else {
                            None
                        },
                    )
                    .await;
            });
        }
        let _ = REDIS
            .xaddex::<String>(
                &format!("nebula:bridge:{}:stream", &self.id),
                &ChannelEvent::BridgeHangup.to_string(),
                &result.to_string(),
                60,
            )
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}", &self.id), 60)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:stream", &self.id), 60)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:channels", &self.id), 60)
            .await;
        let _ = REDIS
            .expire(&format!("nebula:bridge:{}:hangup_channels", &self.id), 60)
            .await;

        let bridge_id = self.id.clone();
        tokio::spawn(async move {
            let bridge = Bridge::new(bridge_id);
            let _ = bridge.hangup_ack_check().await;
        });

        if let Ok(room) = REDIS
            .hget::<String>(&format!("nebula:bridge:{}", &self.id), "room")
            .await
        {
            if let Ok(invitation) = REDIS
                .hget::<String>(&format!("nebula:bridge:{}", &self.id), "invitation")
                .await
            {
                let bridge_id = self.id.clone();
                tokio::spawn(async move {
                    let _ = Room::new(room)
                        .notify_room_invite_rejection(bridge_id, invitation)
                        .await;
                });
            }
        }

        let inbound = self.get_inbound().await?;
        let _ = inbound.stop_ringtone().await;

        Ok(())
    }

    async fn hangup_ack_check(&self) -> Result<()> {
        let mut stream_key = "0".to_string();
        loop {
            tokio::select! {
                (key_id, event_name, _event_value) = self.next_event(&stream_key) => {
                    match ChannelEvent::from_str(&event_name)? {
                        ChannelEvent::BridgeHangupAck => {
                            return Ok(());
                        }
                        _ => (),
                    };
                    stream_key = key_id;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    let inbound = self.get_inbound().await?;
                    let _ = inbound.hangup(None).await;
                    println!("hangup ack check failed, hangup inbound channel");
                    return Ok(());
                }
            }
        }
    }
}

fn format_trunk_number(user: &str, to_exten: &str) -> Option<String> {
    let mut parts = user.split('%');
    let keeping = parts.next()?;
    if let Some(pattern) = parts.next() {
        if pattern.to_lowercase() == "e164" {
            let formatted = get_phone_number(to_exten)
                .map(|n| {
                    n.format().mode(phonenumber::Mode::E164).to_string()[1..]
                        .to_string()
                })
                .unwrap_or_else(|| to_exten.to_string());
            return Some(format!("{keeping}{formatted}"));
        } else if let Ok(country) =
            phonenumber::country::Id::from_str(&pattern.to_uppercase())
        {
            let formatted = get_phone_number(to_exten)
                .map(|n| {
                    let cc = n.country().id();
                    if cc == Some(country) {
                        format_national_number(&n)
                    } else {
                        n.format().mode(phonenumber::Mode::E164).to_string()[1..]
                            .to_string()
                    }
                })
                .unwrap_or_else(|| to_exten.to_string());
            return Some(format!("{keeping}{formatted}"));
        }
    }
    Some(user.to_string())
}

fn external_number_location(
    number: &PhoneNumber,
    provider: &str,
    operator_code: &Option<(String, bool, bool)>,
) -> Option<Location> {
    let provider = provider.to_lowercase();
    let provider = provider.as_str();
    let host = match provider {
        "simwood" => "out.simwood.com",
        "twilio" => "yayyay.pstn.twilio.com",
        "gamma" => "88.215.49.225",
        "bt" => ["62.239.63.202", "147.152.17.138"]
            .choose(&mut rand::thread_rng())
            .unwrap(),
        "colt_uk" => ["212.161.122.242", "62.192.27.242"]
            .choose(&mut rand::thread_rng())
            .unwrap(),
        "colt_spain" => "217.110.38.229",
        "colt_fr" => "213.27.162.81",
        "colt_de" => "213.215.176.143",
        "colt_it" => "217.110.38.235",
        "colt_ie" => "84.14.246.222",
        "colt_be" => "212.161.122.244",
        "flowroute" => "eu-west-ldn.sip.flowroute.com",
        "mondotalk" => "sbc-syd-01.mondotalk.com",
        "barritel" => "sipin.barritel.com",
        "magrathea" => "sipipgw.magrathea.net",
        _ => return None,
    };
    let proxy_host = match provider.to_lowercase().as_str() {
        "mondotalk" => SWITCHBOARD_SERVICE.config.au_proxy_host.clone(),
        _ => SWITCHBOARD_SERVICE.config.london_proxy_host().to_string(),
    };
    let route = None;
    let mut e164 = number.format().mode(phonenumber::Mode::E164).to_string();
    let cc = number.code().value();
    let national = number.national().value();
    if cc == 44 {
        match provider {
            "colt_uk" => match national {
                999 => e164 = "+44999".to_string(),
                101 => e164 = "+44101".to_string(),
                112 => e164 = "+44999".to_string(),
                111 => e164 = "+44111".to_string(),
                105 => e164 = "+44105".to_string(),
                119 => e164 = "+44119".to_string(),
                _ => {}
            },
            "colt_ie" => match national {
                999 => e164 = "+353999".to_string(),
                101 => e164 = "+353101".to_string(),
                112 => e164 = "+353999".to_string(),
                111 => e164 = "+353111".to_string(),
                105 => e164 = "+353105".to_string(),
                119 => e164 = "+353119".to_string(),
                _ => {}
            },
            _ => match national {
                999 => e164 = "999".to_string(),
                101 => e164 = "101".to_string(),
                112 => e164 = "999".to_string(),
                111 => e164 = "111".to_string(),
                105 => e164 = "105".to_string(),
                119 => e164 = "119".to_string(),
                _ => {
                    if let Some((operator_code, short, has_operator_number)) =
                        operator_code
                    {
                        if *has_operator_number {
                            // not doing anything if it's an operator number
                        } else if *short {
                            e164 = operator_code.to_string()
                        } else {
                            e164 = format!(
                                "{operator_code}{};phone-context=+44",
                                format_national_number(number)
                            );
                        }
                    }
                }
            },
        }
    } else if cc == 61 && national == 0 {
        e164 = "000".to_string();
    }

    let dst_uri = Uri {
        scheme: "sip".to_string(),
        user: Some(e164.clone()),
        host: host.to_string(),
        ..Default::default()
    };
    let req_uri = if provider == "flowroute" {
        Uri {
            scheme: "sip".to_string(),
            user: Some(format!("24986807*{}", &e164[1..])),
            host: host.to_string(),
            ..Default::default()
        }
    } else if provider == "magrathea"
        && (e164.starts_with("+3531800")
            || e164.starts_with("+3531850")
            || e164.starts_with("+3531890"))
    {
        Uri {
            scheme: "sip".to_string(),
            user: Some(format!("+353*{}", &e164[4..])),
            host: host.to_string(),
            ..Default::default()
        }
    } else {
        Uri {
            scheme: "sip".to_string(),
            user: Some(e164.clone()),
            host: host.to_string(),
            ..Default::default()
        }
    };
    Some(Location {
        user: e164,
        from_host: "talk.yay.com".to_string(),
        to_host: host.to_string(),
        proxy_host,
        req_uri,
        dst_uri,
        display_name: None,
        intra: false,
        inter_tenant: None,
        encryption: false,
        route,
        device: None,
        device_token: None,
        dnd: false,
        dnd_allow_internal: false,
        provider: Some(true),
        is_registration: None,
        user_agent: None,
        tenant_id: None,
        teams_refer_cx: None,
        ziron_tag: None,
        diversion: None,
        pai: None,
        global_dnd: false,
        global_forward: None,
        room_auth_token: None,
    })
}

mod test {
    use crate::bridge::format_trunk_number;

    #[test]
    fn test_trunk_pattern() {
        let user = "%gb";
        let to_exten = "01483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("01483999289".to_string())
        );

        let user = "%e164";
        let to_exten = "01483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("441483999289".to_string())
        );

        let user = "%gb";
        let to_exten = "483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("483999289".to_string())
        );

        let user = "%e164";
        let to_exten = "483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("483999289".to_string())
        );

        let user = "54321%gb";
        let to_exten = "01483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("5432101483999289".to_string())
        );

        let user = "54321%e164";
        let to_exten = "01483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("54321441483999289".to_string())
        );

        let user = "54321%e164";
        let to_exten = "483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("54321483999289".to_string())
        );

        let user = "54321%gb";
        let to_exten = "483999289";
        assert_eq!(
            format_trunk_number(user, to_exten),
            Some("54321483999289".to_string())
        );
    }
}
