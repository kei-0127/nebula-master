use super::server::SWITCHBOARD_SERVICE;
use super::switchboard::Switchboard;
use crate::callqueue::format_queue_member;
use crate::channel::{
    external_caller_id, internal_caller_id, redirect_context, Channel,
};
use crate::conference::Conference;
use crate::switchboard::get_phone_number;
use crate::{
    bridge::{BridgeExtension, ExternalNumber},
    channel::ChannelBridgeResult,
    switchboard::now,
};
use crate::{
    callqueue::{CallQueue, CallQueueResult},
    switchboard::check_time,
};
use anyhow::{anyhow, Result};
use async_recursion::async_recursion;
use chrono::Utc;
use futures::stream::{self, StreamExt};
use itertools::Itertools;
use nebula_db::message::{
    CallConfirmation, CallDirection, ChannelEvent, Endpoint, Response,
};
use nebula_db::models::{
    is_valid_number, CdrItemChange, DiaryRecord, NewFaxItem, QueueRecordChange, User,
};
use nebula_redis::{
    redis::{self},
    REDIS,
};
use serde::Deserialize;
use serde_json::{self, json, Value};
use sip::message::{CallerId, InterTenantInfo};
use std::str::FromStr;
use std::time::UNIX_EPOCH;
use std::{
    collections::{HashMap, HashSet},
    time::SystemTime,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub enum ConferenceAuth {
    Member,
    Chairman,
    Failed,
}

#[derive(Deserialize)]
struct FaxResult {
    page: usize,
    status: String,
    code: usize,
}

pub struct CallFlow {
    id: String,
    switchboard: Switchboard,
    flow: HashMap<String, Value>,
    tenant_uuid: String,
    unlimited_recording: bool,
    n: Mutex<usize>,
}

#[derive(Debug)]
pub struct OffnetConnect {
    pub pin: String,
    pub caller_id: Option<String>,
    pub prompt: Option<String>,
    pub allowed_cli_list: Vec<String>,
    pub pin_timeout: Option<u64>,
    pub tenant_id: String,
}

impl CallFlow {
    pub fn new(
        id: String,
        switchboard: Switchboard,
        flow: HashMap<String, Value>,
        tenant_uuid: String,
        unlimited_recording: bool,
    ) -> Self {
        CallFlow {
            id,
            switchboard,
            flow,
            tenant_uuid,
            unlimited_recording,
            n: Mutex::new(0),
        }
    }

    #[async_recursion]
    async fn switch(&self, module_key: &str) -> Result<()> {
        let mut module_key = module_key.to_string();
        loop {
            if self.switchboard.channel.is_hangup().await {
                return Err(anyhow!("channel has hangup"));
            }
            let module: HashMap<String, Value> = serde_json::from_value(
                self.flow
                    .get(&module_key)
                    .ok_or_else(|| anyhow!("can't find module key {}", module_key))?
                    .clone(),
            )?;

            self.switchboard
                .channel
                .new_event(ChannelEvent::SwitchCallflow, &module_key)
                .await?;

            {
                let mut n = self.n.lock().await;
                *n += 1;
                if *n > 1000 {
                    return Ok(());
                }
            }
            let module = &module;
            info!(
                channel = self.switchboard.channel.id,
                "callflow now switch {module:?}",
            );
            module_key = match self.module_name(module)?.as_ref() {
                "musiconhold" => self.switch_musiconhold(module).await?,
                "extension" => self.switch_extension(module).await?,
                "pressextension" => self.switch_pressextension(module).await?,
                "dialbyextension" => self.switch_dialbyextension(module).await?,
                "backupextension" => self.switch_backupextension(module).await?,
                "forwardcall" => self.switch_forward_call(module).await?,
                "room" => self.switch_room(module).await?,
                "conference" => self.switch_conference(module).await?,
                "play" => self.switch_play(module).await?,
                "recording" => self.switch_recording(module).await?,
                "voicemail" => self.switch_voicemail(module).await?,
                "callqueue" => self.switch_callqueue(module).await?,
                "callflow" => self.switch_callflow(module).await?,
                "diary" => self.switch_diary(module).await?,
                "callflowcontent" => self.switch_callflow_content(module).await?,
                "outofhours" => self.switch_outofhours(module).await?,
                "trunk" => self.switch_trunk(module).await?,
                "busy" => self.switch_busy(module).await?,
                "sleep" => self.switch_sleep(module).await?,
                "fax" => self.switch_fax(module).await?,
                "calleridmatch" => self.switch_callerid_match(module).await?,
                "offnetconnect" => self.switch_offnet_connect(module).await?,
                _ => Err(anyhow!("doesn't support module"))?,
            };
        }
    }

    fn module_name(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        Ok(self.module_value(module, "module")?.to_lowercase())
    }

    fn module_value(
        &self,
        module: &HashMap<String, serde_json::Value>,
        key: &str,
    ) -> Result<String> {
        Ok(module
            .get(key)
            .ok_or_else(|| anyhow!("can't find key {}", key))?
            .as_str()
            .ok_or_else(|| anyhow!("module value is not string"))?
            .to_string())
    }

    pub async fn start(&self) -> Result<()> {
        println!("start callflow");

        for (_, module) in self.flow.iter() {
            if let Ok(module) = serde_json::from_value(module.clone()) {
                if let Ok(name) = self.module_name(&module) {
                    if name == "start" {
                        let next = self.module_value(&module, "next")?;
                        self.switch(&next).await?;
                        return Ok(());
                    }
                }
            }
        }

        println!("no start");
        Ok(())
    }

    async fn switch_musiconhold(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let moh_class = self.module_value(module, "class")?;
        let mut context = self.switchboard.channel.get_call_context().await?;
        context.call_flow_moh = Some(moh_class);
        self.switchboard.channel.set_call_context(&context).await?;
        self.module_value(module, "next")
    }

    async fn current_module(
        &self,
    ) -> Result<(usize, HashMap<String, serde_json::Value>)> {
        let events: Vec<redis::Value> = REDIS
            .xrevrange(
                &format!("nebula:channel:{}:stream", &self.switchboard.channel.id),
                "+",
                "-",
            )
            .await?;
        for value in events.iter() {
            let (key, event): (String, Vec<String>) =
                redis::from_redis_value(value)?;
            if event[0] == ChannelEvent::SwitchCallflow.to_string() {
                let module_key = &event[1];
                let module = self.flow.get(module_key).ok_or_else(|| {
                    anyhow!("can't find key {} in module", module_key)
                })?;
                let module = serde_json::from_value(module.clone())?;
                let time = key.split('-').next().unwrap().parse::<usize>()?;
                return Ok((time, module));
            }
        }
        Err(anyhow!("can't find current call module"))
    }

    async fn format_extension(
        &self,
        extension: &serde_json::Value,
    ) -> Option<BridgeExtension> {
        let extension_type = extension.get("type")?.as_str()?.to_lowercase();
        match extension_type.as_ref() {
            "sipuser" => {
                let sipuser_uuid = extension.get("uuid")?.as_str()?;
                let user = SWITCHBOARD_SERVICE
                    .db
                    .get_user(&self.tenant_uuid, sipuser_uuid)
                    .await?;
                Some(BridgeExtension::User(user))
            }
            "huntgroup" => {
                let group_uuid = extension.get("uuid")?.as_str()?;
                let group = SWITCHBOARD_SERVICE
                    .db
                    .get_group(&self.tenant_uuid, group_uuid)
                    .await?;
                Some(BridgeExtension::Group(group))
            }
            "trunk" => {
                let context =
                    self.switchboard.channel.get_call_context().await.ok()?;
                let trunk_uuid = extension.get("uuid")?.as_str()?;
                let trunk = SWITCHBOARD_SERVICE
                    .db
                    .get_trunk(&self.tenant_uuid, trunk_uuid)
                    .await?;
                Some(BridgeExtension::Trunk(
                    trunk,
                    context.ziron_tag,
                    context.diversion,
                    context.pai,
                ))
            }
            // Used for normal inter tenant calling
            "inter_tenant" => {
                let context =
                    self.switchboard.channel.get_call_context().await.ok()?;

                let exten = extension.get("exten")?.as_str()?.to_string();
                let src_tenant_id = extension.get("src_tenant_id")?.as_str()?;
                let dst_tenant_id = extension.get("dst_tenant_id")?.as_str()?;
                let from_exten = extension.get("from_exten")?.as_str()?;
                let from_display_name =
                    extension.get("from_display_name")?.as_str()?;

                // detect forward chain loop
                if context.intra_chain.contains(&exten) {
                    return None;
                }

                Some(BridgeExtension::InterTenant(InterTenantInfo {
                    src_tenant_id: src_tenant_id.to_string(),
                    dst_tenant_id: dst_tenant_id.to_string(),
                    exten,
                    from_exten: from_exten.to_string(),
                    from_display_name: from_display_name.to_string(),
                    extern_call_flow: false,
                }))
            }
            // Used when frontend makes a callflow to an external account
            "account_bridge" => {
                let context =
                    self.switchboard.channel.get_call_context().await.ok()?;

                let exten = extension.get("number")?.as_str()?.to_string();
                let dst_tenant_id = extension.get("uuid")?.as_str()?;

                // detect forward chain loop
                if context.intra_chain.contains(&exten) {
                    return None;
                }

                // Need to check if this is provider only
                let from_exten = match context.endpoint.clone() {
                    Endpoint::User { user, .. } => user
                        .extension
                        .map(|ex| ex.to_string())
                        .unwrap_or(String::new()),
                    Endpoint::Trunk { number, .. } => number,
                    Endpoint::Provider { number, .. } => number,
                };

                Some(BridgeExtension::InterTenant(InterTenantInfo {
                    src_tenant_id: self.tenant_uuid.clone(),
                    from_exten,
                    from_display_name: context
                        .call_flow_name
                        .unwrap_or("External Flow".to_string()),
                    dst_tenant_id: dst_tenant_id.to_string(),
                    exten,
                    extern_call_flow: true,
                }))
            }
            "forward" => {
                let context =
                    self.switchboard.channel.get_call_context().await.ok()?;
                let cc = {
                    let cc = context.endpoint.cc();
                    if cc.is_empty() {
                        None
                    } else {
                        phonenumber::country::Id::from_str(&cc).ok()
                    }
                }
                .unwrap_or(phonenumber::country::GB);

                let exten = extension.get("number")?.as_str()?;
                let (operator_code, exten) = get_operator_code(cc, exten);
                let number = phonenumber::parse(Some(cc), exten).ok()?;
                if operator_code
                    .as_ref()
                    .map(|(_, short_operator_code)| *short_operator_code)
                    .unwrap_or(false)
                {
                    println!("short operator code, we don't need to check if valid");
                } else if !is_valid_number(&number) {
                    println!("number not valid");
                    return None;
                }

                let e164 = number.format().mode(phonenumber::Mode::E164).to_string();

                // detect forward chain loop
                if context.intra_chain.contains(&e164) {
                    return None;
                }

                if let Some(to_number) = get_phone_number(&context.to) {
                    // don't foward a number to itself
                    if to_number == number {
                        let outbound_call = extension
                            .get("outbound_call")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if !outbound_call {
                            info!(
                                channel = self.switchboard.channel.id,
                                "don't forward number to itself {}",
                                self.switchboard.channel.id
                            );
                            return None;
                        }
                    }
                }

                let confirmation = if extension
                    .get("confirm_enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    Some(CallConfirmation {
                        key: extension
                            .get("confirm_key")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "".to_string()),
                        sound: extension
                            .get("confirm_file")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "".to_string()),
                        timeout: extension
                            .get("confirm_timeout")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                    })
                } else {
                    None
                };

                let mut user_agent: Option<String> = None;

                if let Some(internal_number) =
                    SWITCHBOARD_SERVICE.db.get_number("admin", &e164[1..]).await
                {
                    let (is_trunk, user_uuid) = match &context.endpoint {
                        Endpoint::Trunk { .. } => (true, None),
                        Endpoint::Provider { .. } => (false, None),
                        Endpoint::User {
                            user,
                            user_agent_type,
                            ..
                        } => {
                            user_agent =
                                user_agent_type.as_ref().map(|t| t.to_string());
                            (false, Some(&user.uuid))
                        }
                    };
                    let start_time = std::time::Instant::now();
                    let request_json = serde_json::json!({
                        "reseller_uuid": &self.tenant_uuid,
                        "country_code": number.code().value(),
                        "destination": number.national().value(),
                        "e164": e164,
                        "trunk": is_trunk,
                        "user_uuid": user_uuid,
                        "user_agent_type": user_agent,
                        "is_transfer": context.refer.map(|r| r.direction) == Some(CallDirection::Recipient),
                    });
                    info!(
                        channel = self.switchboard.channel.id,
                        "[{}] create call request {request_json}",
                        &self.switchboard.channel.id,
                    );
                    let (code, resp) = match self
                        .switchboard
                        .yaypi_client
                        .check_outbound_restriction(request_json)
                        .await
                    {
                        Ok((code, resp)) => (code, resp),
                        Err(e) => {
                            info!(
                                channel = self.switchboard.channel.id,
                                "[{}] create call error {e:?}, took {}ms",
                                &self.switchboard.channel.id,
                                start_time.elapsed().as_millis(),
                            );
                            return None;
                        }
                    };
                    info!(
                        channel = self.switchboard.channel.id,
                        "[{}] create call done {} {}, took {}ms",
                        &self.switchboard.channel.id,
                        code,
                        resp,
                        start_time.elapsed().as_millis(),
                    );
                    if code == 402
                        && context.endpoint.user().is_some()
                        && extension
                            .get("outbound_call")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                    {
                        let _ = self.switchboard.channel.answer().await;
                        let msg = resp
                            .get("message")
                            .and_then(|m| m.as_str())
                            .map(|m| m.to_string())
                            .unwrap_or_else(|| "".to_string());
                        if !msg.is_empty() {
                            let _ = self
                                .switchboard
                                .channel
                                .play_sound(
                                    None,
                                    vec![format!("tts://{}", &msg)],
                                    true,
                                )
                                .await;
                        }
                    }
                    if code != 200 {
                        return None;
                    }
                    return Some(BridgeExtension::Intra(
                        internal_number,
                        confirmation,
                    ));
                }

                let (is_trunk, external_caller_id, user_uuid, trunk_uuid) =
                    match &context.endpoint {
                        Endpoint::Trunk { trunk, .. } => {
                            let caller_id = external_caller_id(&context).await;
                            (true, caller_id, None, Some(&trunk.uuid))
                        }
                        Endpoint::User {
                            user,
                            user_agent_type,
                            ..
                        } => {
                            user_agent =
                                user_agent_type.as_ref().map(|t| t.to_string());
                            let redirect_context = redirect_context(&context).await;
                            let callerid_context =
                                redirect_context.as_ref().unwrap_or(&context);
                            let caller_id =
                                external_caller_id(callerid_context).await;
                            (
                                false,
                                caller_id,
                                if context.redirect_peer.is_some() {
                                    // if it's a redirect
                                    // we don't set user_uuid so that
                                    // the original caller id don't get overwritten
                                    // by yaypi create_call
                                    None
                                } else {
                                    Some(user.uuid.as_str())
                                },
                                None,
                            )
                        }
                        Endpoint::Provider { .. } => {
                            let caller_id = external_caller_id(&context).await;
                            (false, caller_id, None, None)
                        }
                    };
                info!(
                    channel = self.switchboard.channel.id,
                    "[{}] now create call, callerid {}",
                    &self.switchboard.channel.id,
                    external_caller_id.user,
                );
                let cdr_uuid =
                    if let Some(redirect) = redirect_context(&context).await {
                        redirect.cdr.as_ref()?.to_string()
                    } else {
                        context.cdr.as_ref()?.to_string()
                    };

                let start_time = std::time::Instant::now();

                let request_json = serde_json::json!({
                    "uuid": &cdr_uuid,
                    "reseller_uuid": &self.tenant_uuid,
                    "operator_code": operator_code.map(|(c, _)| c.to_string()),
                    "country_code": number.code().value(),
                    "destination": number.national().value(),
                    "e164": e164,
                    "ip_address": context.remote_ip,
                    "trunk": is_trunk,
                    "callerid": Some(&external_caller_id.user),
                    "user_uuid": user_uuid,
                    "user_agent_type": user_agent,
                    "trunk_uuid": trunk_uuid,
                    "remove_call_restrictions": context.remove_call_restrictions,
                    "is_transfer": context.refer.map(|r| r.direction) == Some(CallDirection::Recipient),
                });
                info!(
                    channel = self.switchboard.channel.id,
                    "[{}] create call request {request_json}",
                    &self.switchboard.channel.id,
                );
                let (code, resp) = self
                    .switchboard
                    .yaypi_client
                    .create_call(request_json)
                    .await
                    .ok()?;
                info!(
                    channel = self.switchboard.channel.id,
                    "[{}] create call done {} {}, took {}ms",
                    &self.switchboard.channel.id,
                    code,
                    resp,
                    start_time.elapsed().as_millis(),
                );
                if code != 200 {
                    if code == 402 {
                        let outbound_call = extension
                            .get("outbound_call")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if outbound_call {
                            if let Some(user) = context.endpoint.user() {
                                let _ = self.switchboard.channel.answer().await;
                                let mut msg = resp
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .map(|m| m.to_string())
                                    .unwrap_or_else(|| "".to_string());
                                if msg.is_empty() {
                                    if user
                                        .domain
                                        .as_ref()
                                        .unwrap_or(&"".to_string())
                                        == "talk.yay.com"
                                    {
                                        msg = "You do not have sufficient call credit or active call packages to make this call. Please log in to your account at Yay.com and visit My VoIP, and then Package, to apply call credit or amend your packages.".to_string();
                                    } else {
                                        msg = "No call credit to make this call."
                                            .to_string();
                                    }
                                }
                                let _ = self
                                    .switchboard
                                    .channel
                                    .play_sound(
                                        None,
                                        vec![format!("tts://{}", &msg)],
                                        true,
                                    )
                                    .await;
                            }
                        }
                    }
                    return None;
                }

                {
                    let msg =
                        resp.get("message").and_then(|m| m.as_str()).unwrap_or("");
                    if !msg.is_empty() {
                        let _ = self
                            .switchboard
                            .channel
                            .play_sound(None, vec![format!("tts://{}", msg)], true)
                            .await;
                    }
                }

                let provider = resp["provider"].as_str()?.to_string();
                let backup_provider = resp
                    .get("backup_provider")
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string())
                    .filter(|v| v != &provider);

                let callerid = resp
                    .get("callerid")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&external_caller_id.user);
                let caller_id = CallerId {
                    user: callerid.to_string(),
                    e164: None,
                    display_name: "".to_string(),
                    anonymous: external_caller_id.anonymous,
                    asserted_identity: if provider == "barritel" {
                        external_caller_id.original_number.clone().or_else(|| {
                            resp.get("asserted_identity")
                                .and_then(|v| v.as_str())
                                .map(|v| v.to_string())
                        })
                    } else {
                        resp.get("asserted_identity")
                            .and_then(|v| v.as_str())
                            .map(|v| v.to_string())
                    },
                    original_number: None,
                    to_number: external_caller_id.to_number.clone(),
                };

                let backup_callerid = resp
                    .get("backup_callerid")
                    .and_then(|v| v.as_str())
                    .unwrap_or(callerid);
                let backup_caller_id = CallerId {
                    user: backup_callerid.to_string(),
                    e164: None,
                    display_name: "".to_string(),
                    anonymous: external_caller_id.anonymous,
                    asserted_identity: resp
                        .get("asserted_identity")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string()),
                    original_number: None,
                    to_number: external_caller_id.to_number.clone(),
                };

                let number = if operator_code.is_some() {
                    resp.get("operator_number")
                        .and_then(|v| v.as_str())
                        .and_then(|s| {
                            let s = if !s.starts_with('+') && !s.starts_with('0') {
                                format!("+{}", s)
                            } else {
                                s.to_string()
                            };
                            phonenumber::parse(Some(cc), s).ok()
                        })
                        .unwrap_or(number)
                } else {
                    number
                };

                let number = ExternalNumber {
                    tenant_id: self.tenant_uuid.clone(),
                    number,
                    provider,
                    backup_provider,
                    caller_id: Some(caller_id),
                    backup_caller_id: Some(backup_caller_id),
                    operator_code: operator_code.map(|(c, s)| {
                        (c.to_string(), s, resp.get("operator_number").is_some())
                    }),
                };
                Some(BridgeExtension::ExternalNumber(number, confirmation))
            }
            &_ => None,
        }
    }

    async fn conference_pin_auth(
        &self,
        pin: &str,
        chairman_pin: &str,
    ) -> Result<ConferenceAuth> {
        let mut n = 0;
        loop {
            self.switchboard
                .channel
                .new_event(ChannelEvent::StartDtmf, "pin")
                .await?;
            let digits =
                self.switchboard
                    .channel
                    .play_sound_and_get_dtmf(
                        vec!["tts://Please enter the conference pin number."
                            .to_string()],
                        5000,
                        5000,
                        pin.len(),
                    )
                    .await;
            println!("digits {:?}", &digits);
            if digits.join("") == chairman_pin {
                return Ok(ConferenceAuth::Chairman);
            }
            if digits.join("") == pin {
                return Ok(ConferenceAuth::Member);
            }
            println!("invalid pin");
            n += 1;
            if n > 10 {
                return Ok(ConferenceAuth::Failed);
            }
            self.switchboard
                .channel
                .play_sound(
                    None,
                    vec!["tts://Invalid pin number, try again.".to_string()],
                    true,
                )
                .await?;
            println!("invalid pin played done");
        }
    }

    async fn switch_room(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        self.switchboard.channel.answer().await?;

        let rooms: HashMap<String, String> = serde_json::from_value(
            module
                .get("rooms")
                .ok_or_else(|| anyhow!("no rooms"))?
                .clone(),
        )?;

        let max_len = rooms.iter().map(|(pin, _)| pin.len()).max().unwrap_or(0);

        let mut n = 0;
        loop {
            let digits =
                self.switchboard
                    .channel
                    .play_sound_and_get_dtmf(
                        vec!["tts://Please enter the conference pin number."
                            .to_string()],
                        5000,
                        5000,
                        max_len,
                    )
                    .await;
            if let Some(_room) = rooms.get(&digits.join("")) {
                return Err(anyhow!("no next module"));
            }
            n += 1;
            if n > 10 {
                break;
            }
            self.switchboard
                .channel
                .play_sound(
                    None,
                    vec!["tts://Invalid pin number, try again.".to_string()],
                    true,
                )
                .await?;
        }

        Err(anyhow!("no next module"))
    }

    async fn switch_conference(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        println!("switch conference");
        self.switchboard.channel.answer().await?;

        let no_joinleave = module
            .get("no_joinleave")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let pin = self
            .module_value(module, "pin")
            .unwrap_or_else(|_| "".to_string());
        let chairman_pin = self
            .module_value(module, "chairman_pin")
            .unwrap_or_else(|_| "".to_string());
        let mut is_chairman = false;
        if !pin.is_empty() || !chairman_pin.is_empty() {
            match self.conference_pin_auth(&pin, &chairman_pin).await? {
                ConferenceAuth::Member => {}
                ConferenceAuth::Chairman => {
                    is_chairman = true;
                }
                ConferenceAuth::Failed => return Err(anyhow!("no next module")),
            }
        }

        if !no_joinleave {
            let audio_string = r#"tts://After the tone please record your name, press any key or stop talking to end the recording<break time="1ms" />"#.to_string();
            self.switchboard
                .channel
                .play_sound(
                    None,
                    vec![audio_string, "tone://200,0,500,0".to_string()],
                    true,
                )
                .await?;
            self.switchboard
                .channel
                .record_to_redis(
                    format!("{}-name", &self.switchboard.channel.id),
                    3000,
                    86400,
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(3000)).await;
        }

        if let Ok(announcement) = self.module_value(module, "announcement") {
            self.switchboard
                .channel
                .play_sound(None, vec![announcement.to_string()], true)
                .await?;
        }

        let conference_id =
            if let Ok(value) = self.module_value(module, "original_callflow") {
                value
            } else {
                self.id.clone()
            };

        let moh = if let Some(moh_id) = module.get("music").and_then(|s| s.as_str())
        {
            SWITCHBOARD_SERVICE.db.get_music_on_hold(moh_id).await
        } else {
            None
        };
        let conference = Conference::new(
            conference_id,
            no_joinleave,
            !chairman_pin.is_empty(),
            moh,
        );
        let channel = self.switchboard.channel.clone();
        conference.join(&channel, is_chairman).await?;
        conference.wait(&channel).await?;

        Err(anyhow!("no next module"))
    }

    async fn switch_recording(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        self.switchboard
            .channel
            .update_recording(&self.tenant_uuid, true, self.unlimited_recording)
            .await?;
        self.module_value(module, "next")
    }

    async fn switch_play(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        println!("switch play sound");
        if let Err(e) = self.switchboard.channel.answer().await {
            println!("answer error {}", e);
        }
        let sound = self.module_value(module, "sound")?;
        self.switchboard
            .channel
            .play_sound(None, vec![sound], true)
            .await?;
        self.module_value(module, "next")
    }

    fn voicemail_mailbox(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Option<String> {
        println!("voicemail module {:?}", module);
        let mailboxes = module.get("mailboxes")?.as_array()?;
        if mailboxes.is_empty() {
            return None;
        }
        mailboxes[0].as_str().map(|m| m.to_string())
    }

    async fn switch_callqueue(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let groups = module
            .get("groups")
            .ok_or_else(|| anyhow!("don't have groups"))?
            .as_array()
            .ok_or_else(|| anyhow!("group not array"))?;
        if groups.is_empty() {
            println!("queue group is empty");
            return Err(anyhow!("no next module"));
        }
        let group_uuid = groups[0]
            .as_str()
            .ok_or_else(|| anyhow!("group not string"))?;
        let group = SWITCHBOARD_SERVICE
            .db
            .get_queue_group(group_uuid)
            .await
            .ok_or_else(|| anyhow!("no such queue group {}", group_uuid))?;

        let moh = if let Some(moh_id) =
            module.get("hold_music").and_then(|s| s.as_str())
        {
            SWITCHBOARD_SERVICE.db.get_music_on_hold(moh_id).await
        } else {
            None
        };

        let hint = if let Some(hint_id) = module.get("hint").and_then(|s| s.as_str())
        {
            SWITCHBOARD_SERVICE.db.get_queue_hint(hint_id).await
        } else {
            None
        };

        let members: serde_json::Value = serde_json::from_str(&group.members)?;
        let raw_members = members
            .as_array()
            .ok_or_else(|| anyhow!("members is not array"))?
            .to_owned();
        let members: Vec<BridgeExtension> = stream::iter(&raw_members)
            .filter_map(|m| async move {
                format_queue_member(&self.tenant_uuid, m).await
            })
            .collect::<Vec<Vec<BridgeExtension>>>()
            .await
            .into_iter()
            .flatten()
            .unique_by(|e| e.id())
            .collect();

        self.switchboard.channel.answer().await?;
        let queue_record = SWITCHBOARD_SERVICE
            .db
            .create_queue_record(self.id.clone(), group_uuid.to_string())
            .await?;
        let mut context = self.switchboard.channel.get_call_context().await?;
        context.call_queue_id = Some(group_uuid.to_string());
        context.call_queue_name = Some(group.name.clone());
        self.switchboard.channel.set_call_context(&context).await?;

        let local_channel = self.switchboard.channel.clone();
        let local_queue_record = queue_record.clone();
        tokio::spawn(async move {
            let mut cdr_wait = 0;

            let (cdr_uuid, cdr_answer) =
                if let Some(cdr) = local_channel.get_cdr().await {
                    let cdr_answer = if let Some(answer) = cdr.answer.as_ref() {
                        cdr_wait = answer
                            .signed_duration_since(local_queue_record.start)
                            .num_seconds();
                        Some(*answer)
                    } else {
                        None
                    };
                    (Some(cdr.uuid), cdr_answer)
                } else {
                    (None, None)
                };
            SWITCHBOARD_SERVICE
                .db
                .update_queue_record(
                    local_queue_record.uuid,
                    QueueRecordChange {
                        cdr: cdr_uuid.map(|u| u.to_string()),
                        cdr_start: cdr_answer,
                        cdr_wait: Some(cdr_wait),
                        ..Default::default()
                    },
                )
                .await?;
            Ok::<(), anyhow::Error>(())
        });

        let start_time = SystemTime::now();
        let start_time_unix = start_time
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .to_string();
        let queue = CallQueue {
            id: group_uuid.to_string(),
            tenant_id: self.tenant_uuid.clone(),
            record_id: queue_record.uuid,
            switchboard: self.switchboard.clone(),
            group: group.clone(),
            start_time,
            start_time_unix,
            waiting_time: module
                .get("waiting_time_duration")
                .unwrap_or(&serde_json::to_value(0)?)
                .as_f64()
                .unwrap_or(0.0) as u64,
            ring_timeout: module
                .get("ring_timeout")
                .and_then(|t| t.as_f64())
                .unwrap_or(0.0) as u64,
            max_waiting: module
                .get("max_waiting")
                .and_then(|t| t.as_u64())
                .unwrap_or(0),
            timeout_time: module
                .get("queue_timeout")
                .and_then(|t| t.as_f64())
                .map(|t| t as u64)
                .and_then(|t| if t > 0 { Some(t) } else { None })
                .map(|t| start_time + Duration::from_secs(t)),
            shortcode: module
                .get("shortcode")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            callback: module
                .get("callback")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            moh,
            hint,
            ring_audio: module
                .get("ring_audio")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            ringtone: module
                .get("ringtone")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            announcement: module
                .get("announcement")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            no_registered: module
                .get("no_registered")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            tried_members: Arc::new(Mutex::new(HashSet::new())),
            answer_at: Arc::new(Mutex::new(None)),
            failed_users: Arc::new(Mutex::new(HashMap::new())),
            failed_times: Arc::new(Mutex::new(0)),
            queue_members: Arc::new(Mutex::new(members)),
            raw_members,
            member_hunt_group_updated: Arc::new(Mutex::new(None)),
            in_callback: Arc::new(Mutex::new(false)),
        };
        queue.add_call().await?;
        let init_pos = queue.query_position().await.unwrap_or(0);
        let result = queue.run().await;
        let hangup_pos = queue.query_position().await.unwrap_or(0);

        // Run a stop_moh regardless, becuase in the queue ringing stage,
        // if there's no avaialble agents,
        // it will paly moh.
        // And if the bridge loop got returned because of timeout,
        // there's nothing to stop the moh.
        let _ = self.switchboard.channel.stop_moh().await;

        queue.remove_call().await;
        {
            let duration = queue
                .group
                .duration
                .max(queue.group.answer_wait)
                .max(queue.group.reject_wait)
                .max(queue.group.no_answer_wait);
            if duration > 0 {
                let key = &format!("nebula:queue_for_tenant:{}", queue.tenant_id);
                if REDIS.ttl(key).await.unwrap_or(0) < duration {
                    let _ = REDIS.setex(key, duration as u64, "yes").await;
                }
            }
        }
        let result = match result {
            Ok(result) => {
                info!(
                    channel = self.switchboard.channel.id,
                    queue = queue.id,
                    "channel {} queue {} got result {result}",
                    self.switchboard.channel.id,
                    queue.id
                );
                result
            }
            Err(e) => {
                warn!(
                    channel = self.switchboard.channel.id,
                    queue = queue.id,
                    "channel {} queue {} run error {e}",
                    self.switchboard.channel.id,
                    queue.id
                );
                return Err(e);
            }
        };
        let disposition = match result {
            CallQueueResult::Ok => "answered",
            CallQueueResult::Pickup => "answered",
            CallQueueResult::Timeout => "timeout",
            CallQueueResult::RingTimeout => "ring_timeout",
            CallQueueResult::MaxWaitingReached => "max_waiting_reached",
            CallQueueResult::NoRegistered => "noregistered",
            CallQueueResult::Shortcode => "shortcode",
            CallQueueResult::Fail => "abandoned",
        };
        let failed_users = { queue.failed_users.lock().await.clone() };
        let failed_times = { *queue.failed_times.lock().await };
        let end = Utc::now();

        let local_channel = self.switchboard.channel.clone();
        let tenant_id = self.tenant_uuid.clone();
        let group_id = queue.id.clone();
        tokio::spawn(async move {
            let queue_cdr = SWITCHBOARD_SERVICE.db.create_cdr().await.ok();
            if let Some(queue_cdr) = queue_cdr.clone() {
                if let Some(cdr) = local_channel.get_cdr().await {
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .update_cdr(
                            queue_cdr.uuid,
                            CdrItemChange {
                                start: Some(queue_record.start),
                                tenant_id: Some(tenant_id.clone()),
                                answer: queue_record.answer,
                                end: queue_record.end,
                                disposition: queue_record.disposition,
                                duration: queue_record.duration,
                                billsec: queue_record.call_duration,
                                from_uuid: cdr.from_uuid,
                                from_exten: cdr.from_exten,
                                from_type: cdr.from_type,
                                from_user: cdr.from_user,
                                from_name: cdr.from_name,
                                to_uuid: Some(group.uuid.clone()),
                                to_type: Some("queue".to_string()),
                                to_name: Some(group.name.clone()),
                                answer_uuid: cdr.answer_uuid,
                                answer_exten: cdr.answer_exten,
                                answer_type: cdr.answer_type,
                                answer_user: cdr.answer_user,
                                answer_name: cdr.answer_name,
                                callflow_uuid: cdr.callflow_uuid,
                                callflow_name: cdr.callflow_name,
                                call_type: cdr.call_type,
                                parent: Some(cdr.uuid.to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                    let queue_children = match cdr.queue_children {
                        Some(children) => {
                            if children.is_empty() {
                                queue_cdr.uuid.to_string()
                            } else {
                                format!("{children},{}", queue_cdr.uuid)
                            }
                        }
                        None => queue_cdr.uuid.to_string(),
                    };
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .update_cdr(
                            cdr.uuid,
                            CdrItemChange {
                                queue_children: Some(queue_children),
                                ..Default::default()
                            },
                        )
                        .await;
                }
            }

            let mut queue_record = SWITCHBOARD_SERVICE
                .db
                .update_queue_record(
                    queue_record.uuid,
                    QueueRecordChange {
                        end: Some(end),
                        disposition: Some(disposition.to_string()),
                        duration: Some(
                            end.signed_duration_since(queue_record.start)
                                .num_seconds()
                                .max(0),
                        ),
                        failed_times: Some(failed_times as i64),
                        init_pos: Some(init_pos as i64),
                        hangup_pos: Some(hangup_pos as i64),
                        ..Default::default()
                    },
                )
                .await?;

            for (m, times) in failed_users.iter() {
                let _ = SWITCHBOARD_SERVICE
                    .db
                    .create_queue_failed_user(
                        queue_record.start,
                        group_id.clone(),
                        m.to_string(),
                        *times as i64,
                        queue_record.uuid.to_string(),
                    )
                    .await;
            }

            let mut queue_record_change = QueueRecordChange {
                ..Default::default()
            };
            let mut queue_record_changed = false;
            if let Some(ready) = queue_record.ready {
                queue_record_changed = true;
                queue_record_change.wait_duration = Some(
                    ready
                        .signed_duration_since(queue_record.start)
                        .num_seconds(),
                );
                if let Some(answer) = queue_record.answer {
                    queue_record_change.ring_duration =
                        Some(answer.signed_duration_since(ready).num_seconds());
                }
            }
            if let Some(answer) = queue_record.answer {
                queue_record_changed = true;
                queue_record_change.call_duration =
                    Some(end.signed_duration_since(answer).num_seconds().max(0));
            }
            if result == CallQueueResult::Timeout {
                queue_record_changed = true;
                queue_record_change.wait_duration = Some(
                    end.signed_duration_since(queue_record.start).num_seconds(),
                );
            }
            if queue_record_changed {
                queue_record = SWITCHBOARD_SERVICE
                    .db
                    .update_queue_record(queue_record.uuid, queue_record_change)
                    .await?;
            }

            if let Some(queue_cdr) = queue_cdr {
                if let Some(cdr) = local_channel.get_cdr().await {
                    let _ = SWITCHBOARD_SERVICE
                        .db
                        .update_cdr(
                            queue_cdr.uuid,
                            CdrItemChange {
                                start: Some(queue_record.start),
                                tenant_id: Some(tenant_id),
                                answer: queue_record.answer,
                                end: queue_record.end,
                                disposition: queue_record.disposition,
                                duration: queue_record.duration,
                                billsec: queue_record.call_duration,
                                from_uuid: cdr.from_uuid,
                                from_exten: cdr.from_exten,
                                from_type: cdr.from_type,
                                from_user: cdr.from_user,
                                from_name: cdr.from_name,
                                to_uuid: Some(group.uuid.clone()),
                                to_type: Some("queue".to_string()),
                                to_name: Some(group.name.clone()),
                                answer_uuid: cdr.answer_uuid,
                                answer_exten: cdr.answer_exten,
                                answer_type: cdr.answer_type,
                                answer_user: cdr.answer_user,
                                answer_name: cdr.answer_name,
                                callflow_uuid: cdr.callflow_uuid,
                                callflow_name: cdr.callflow_name,
                                call_type: cdr.call_type,
                                parent: Some(cdr.uuid.to_string()),
                                ..Default::default()
                            },
                        )
                        .await;
                }
            }
            Ok::<(), anyhow::Error>(())
        });

        match result {
            CallQueueResult::Ok | CallQueueResult::Pickup => {
                if let Some(answer) = module.get("answer").and_then(|s| s.as_str()) {
                    return Ok(answer.to_string());
                }
            }
            CallQueueResult::Timeout => {
                if let Some(timeout) = module.get("timeout").and_then(|s| s.as_str())
                {
                    return Ok(timeout.to_string());
                }
            }
            CallQueueResult::MaxWaitingReached => {
                if let Some(max_waiting_module) =
                    module.get("max_waiting_module").and_then(|s| s.as_str())
                {
                    return Ok(max_waiting_module.to_string());
                }
            }
            CallQueueResult::RingTimeout => {
                if let Some(next) =
                    module.get("ring_timeout_module").and_then(|s| s.as_str())
                {
                    return Ok(next.to_string());
                }
            }
            CallQueueResult::NoRegistered => {
                if let Some(no_registered) =
                    module.get("no_registered").and_then(|s| s.as_str())
                {
                    return Ok(no_registered.to_string());
                }
            }
            CallQueueResult::Shortcode => {
                if let Some(shortcode) =
                    module.get("shortcode_module").and_then(|s| s.as_str())
                {
                    return Ok(shortcode.to_string());
                }
            }
            CallQueueResult::Fail => (),
        };
        Err(anyhow!("no next module"))
    }

    async fn switch_callflow_content(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let callflow = module
            .get("callflow_content")
            .ok_or_else(|| anyhow!("no call flow content"))?;
        let callflow = serde_json::from_value(callflow.clone())?;

        let switchboard = self.switchboard.clone();
        let tenant_uuid = self.tenant_uuid.clone();
        let _ = tokio::spawn(async move {
            let _ = switchboard
                .callflow(
                    &tenant_uuid,
                    switchboard.channel.id.clone(),
                    callflow,
                    false,
                )
                .await;
        })
        .await;
        Err(anyhow!("no next module"))
    }

    async fn switch_callflow(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let callflow_id = self.module_value(module, "callflow_uuid")?;
        let callflow = SWITCHBOARD_SERVICE
            .db
            .get_callflow(&self.tenant_uuid, &callflow_id)
            .await
            .ok_or_else(|| anyhow!("no such callflow"))?;

        let switchboard = self.switchboard.clone();
        let _ = tokio::spawn(async move {
            let _ = switchboard.switch_callflow(&callflow).await;
        })
        .await;

        Err(anyhow!("no next module"))
    }

    async fn switch_diary(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let diary_id = self.module_value(module, "diary")?;
        let diary = SWITCHBOARD_SERVICE
            .db
            .get_diary(&self.tenant_uuid, &diary_id)
            .await
            .ok_or_else(|| anyhow!("no such diary"))?;
        let records: Vec<DiaryRecord> = serde_json::from_str(&diary.records)?;
        let ctime = now(if !diary.timezone.is_empty() {
            Some(&diary.timezone)
        } else {
            None
        });

        for record in records {
            if check_time(&ctime, &record.time).unwrap_or(false) {
                let callflow = SWITCHBOARD_SERVICE
                    .db
                    .get_callflow(&self.tenant_uuid, &record.call_flow)
                    .await
                    .ok_or_else(|| anyhow!("no such call flow"))?;
                let switchboard = self.switchboard.clone();
                let _ = tokio::spawn(async move {
                    let _ = switchboard.switch_callflow(&callflow).await;
                })
                .await;
                return Err(anyhow!("no next module"));
            }
        }

        Err(anyhow!("no next module"))
    }

    async fn switch_trunk(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let trunk_uuid = self.module_value(module, "uuid")?;
        let trunk = SWITCHBOARD_SERVICE
            .db
            .get_trunk(&self.tenant_uuid, &trunk_uuid)
            .await
            .ok_or_else(|| anyhow!("no trunk"))?;
        let context = self.switchboard.channel.get_call_context().await?;
        let extension = BridgeExtension::Trunk(
            trunk,
            context.ziron_tag,
            context.diversion,
            context.pai,
        );
        let bridge = self
            .switchboard
            .channel
            .bridge_extensions(
                &self.tenant_uuid,
                vec![&extension],
                "",
                "",
                300,
                None,
                true,
                false,
            )
            .await?;
        let _bridge_result = bridge.wait().await?;
        Err(anyhow!("no next module"))
    }

    async fn switch_callerid_match(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let get_patterns = || -> Option<Vec<String>> {
            let patterns: Vec<String> =
                serde_json::from_value(module.get("patterns")?.clone()).ok()?;
            Some(patterns)
        };

        let callerid_match = if let Some(patterns) = get_patterns() {
            self.switchboard
                .callerid_match_patterns(patterns)
                .await
                .unwrap_or(false)
        } else {
            false
        };

        if callerid_match {
            if let Ok(next) = self.module_value(module, "match") {
                return Ok(next);
            }
        } else if let Ok(next) = self.module_value(module, "nomatch") {
            return Ok(next);
        }

        Err(anyhow!("no next module"))
    }

    async fn switch_offnet_connect(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let unmatched: Result<String> = self
            .module_value(module, "next")
            .map_err(|_| anyhow!("No Next Module"));

        let allowed_cli_list = || -> Option<Vec<String>> {
            let patterns: Vec<String> =
                serde_json::from_value(module.get("allowed_clis")?.clone()).ok()?;
            Some(patterns)
        };

        let Ok(pin) = self.module_value(module, "pin") else {
            return unmatched;
        };
        let Some(allowed_cli_list) = allowed_cli_list() else {
            return unmatched;
        };

        let caller_id = self.module_value(module, "caller_id").ok();
        let prompt = self.module_value(module, "prompt").ok();

        let pin_timeout = match self.module_value(module, "timeout").ok() {
            Some(timeout_str) => timeout_str.parse::<u64>().ok(),
            None => None,
        };

        let offnet_connect = OffnetConnect {
            pin,
            caller_id,
            prompt,
            allowed_cli_list,
            pin_timeout,
            tenant_id: self.tenant_uuid.clone(),
        };

        let Some(number) = self
            .switchboard
            .get_offnet_connect_endpoints(offnet_connect)
            .await?
        else {
            return unmatched;
        };

        let bridge = self
            .switchboard
            .channel
            .bridge_extensions(
                &self.tenant_uuid,
                vec![&self
                    .format_extension(&serde_json::json!({
                        "type": "forward",
                        "number": number,
                    }))
                    .await
                    .ok_or_else(|| anyhow!("can't format extension"))?],
                "",
                "",
                300,
                None,
                true,
                false,
            )
            .await?;
        let _bridge_result = bridge.wait().await?;

        Err(anyhow!("did offnet connect"))
    }

    async fn switch_fax(
        &self,
        _module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        self.switchboard.channel.answer().await?;
        self.switchboard.channel.set_fax().await?;

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(600)) => {}
            _ = self.switchboard.channel.receive_event("0", ChannelEvent::Hangup) => {}
        }

        println!("fax channel hangup");

        let context = self.switchboard.channel.get_call_context().await?;
        let cdr = context.cdr.ok_or_else(|| anyhow!("no cdr"))?;

        let mut page = 0;
        let mut status = "failed".to_string();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            result = self.switchboard.channel
                    .receive_event_ignore_hangup("0", ChannelEvent::FaxEnd) => {
                        println!("fax end result {result:?}");
                        let (_, result) = result?;
                        let result: FaxResult = serde_json::from_str(&result)?;
                        if !result.status.is_empty() {
                            status = result.status.clone();
                        }
                        if result.code == 0 {
                            page = result.page;
                            status = "success".to_string();
                            let key = format!("nebula:fax:{}", self.switchboard.channel.id);
                            if let Ok(s) = REDIS.get::<String>(&key).await {
                                if let Ok(bytes) = base64::decode(&s) {
                                    let _ = storagev1::StorageClient::new()
                                        .upload_content(
                                            "aura-faxes",
                                            &format!("{}.pdf", cdr),
                                            bytes,
                                        )
                                        .await;
                                }
                            }
                        }
                    }
        }

        let _ = SWITCHBOARD_SERVICE
            .db
            .create_fax(NewFaxItem {
                deleted: 0,
                uuid: cdr.to_string(),
                tenant_id: self.tenant_uuid.clone(),
                status: status.clone(),
                folder: "inbox".to_string(),
                to: context.to.clone(),
                callerid: internal_caller_id(&context, None, None)
                    .await
                    .user
                    .clone(),
                time: SystemTime::now(),
                page: page as i64,
            })
            .await;

        let tenant_uuid = self.tenant_uuid.clone();
        let cdr_uuid = cdr.to_string();
        tokio::spawn(async move {
            let _ = SWITCHBOARD_SERVICE
                .yaypi_client
                .notify_fax(&tenant_uuid, &cdr_uuid)
                .await;
        });

        println!("fax done {status}");

        Err(anyhow!("no next module"))
    }

    async fn switch_sleep(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let get_duration = || -> Option<u64> { module.get("duration")?.as_u64() };
        let duration = get_duration().unwrap_or(0);
        if duration > 0 {
            tokio::time::sleep(Duration::from_millis(duration)).await;
        }

        self.module_value(module, "next")
    }

    async fn switch_busy(
        &self,
        _module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        if !self.switchboard.channel.is_answered().await {
            let context = self.switchboard.channel.get_call_context().await?;
            let answer_sdp =
                if let Ok(sdp) = self.switchboard.channel.get_local_sdp().await {
                    sdp
                } else {
                    self.switchboard.channel.create_answer_sdp(false).await?
                };
            if let Some(intra) = context.intra_peer {
                let intra_channel = Channel::new(intra);
                intra_channel
                    .handle_response(&Response {
                        id: "".to_string(),
                        code: 183,
                        status: "Session Progress".to_string(),
                        body: answer_sdp.to_string(),
                        remote_ip: None,
                    })
                    .await?;
            } else {
                SWITCHBOARD_SERVICE
                    .proxy_rpc
                    .set_response(
                        &context.proxy_host,
                        self.switchboard.channel.id.clone(),
                        183,
                        "Session Progress".to_string(),
                        answer_sdp.to_string(),
                    )
                    .await?;
            }
        }
        Channel::play_sound_to_channel(
            &self.switchboard.channel.id,
            None,
            vec![
                "tone://500,500,480,620".to_string(),
                "tone://500,500,480,620".to_string(),
                "tone://500,500,480,620".to_string(),
            ],
            3,
            true,
        )
        .await?;
        Err(anyhow!("no next module"))
    }

    async fn switch_outofhours(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let ctime = now(self.module_value(module, "timezone").ok().as_deref());

        let mut closed = false;
        if let Some(closed_times) = module.get("closed_times") {
            let closed_times: Result<Vec<String>, serde_json::Error> =
                serde_json::from_value(closed_times.clone());
            if let Ok(closed_times) = closed_times {
                for closed_time in closed_times {
                    if check_time(&ctime, &closed_time).unwrap_or(false) {
                        closed = true;
                        break;
                    }
                }
            }
        }

        if let Some(exceptions) = module.get("exceptions") {
            let exceptions: Result<Vec<String>, serde_json::Error> =
                serde_json::from_value(exceptions.clone());
            if let Ok(exceptions) = exceptions {
                for exception in exceptions {
                    if check_time(&ctime, &exception).unwrap_or(false) {
                        closed = false;
                        break;
                    }
                }
            }
        }
        let next = if closed {
            self.module_value(module, "closed")?
        } else {
            self.module_value(module, "open")?
        };
        Ok(next)
    }

    async fn switch_voicemail(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let mailbox = self
            .voicemail_mailbox(module)
            .ok_or_else(|| anyhow!("no mailbox in voicemail"))?;
        let access_code = self
            .module_value(module, "access_code")
            .unwrap_or_else(|_| "**".to_string());
        let voicemail_user = SWITCHBOARD_SERVICE
            .db
            .get_voicemail_user(&self.tenant_uuid, &mailbox)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "can't find mailbox {} for teannt {}",
                    mailbox,
                    self.tenant_uuid
                )
            })?;
        self.switchboard
            .voicemail(&voicemail_user, &access_code)
            .await?;
        Err(anyhow!("no next module"))
    }

    async fn switch_dialbyextension(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let sound = self
            .module_value(module, "sound")
            .unwrap_or_else(|_| "".to_string());
        let get_wait_duration = || -> Option<u64> {
            module.get("wait_duration")?.as_f64().map(|v| v as u64)
        };
        let wait_duration = get_wait_duration().unwrap_or(10) * 1000;
        let digits = self
            .switchboard
            .channel
            .play_sound_and_get_dtmf(vec![sound], wait_duration, 5000, 100)
            .await;

        let get_next = |next: &str| -> Option<&str> {
            module.get("next")?.as_object()?.get(next)?.as_str()
        };
        if digits.is_empty() {
            if let Some(timeout) = get_next("timeout") {
                return Ok(timeout.to_string());
            }
            return Err(anyhow!("no next module"));
        }

        let extension = if let Ok(extension) = digits.join("").parse::<i64>() {
            SWITCHBOARD_SERVICE
                .db
                .get_extension(&self.tenant_uuid, extension)
                .await
        } else {
            None
        };
        match extension {
            None => {
                if let Some(invalid) = get_next("invalid") {
                    return Ok(invalid.to_string());
                }
                return Err(anyhow!("no next module"));
            }
            Some(extension) => match extension.discriminator.as_str() {
                "callflow" => {
                    let callflow = SWITCHBOARD_SERVICE
                        .db
                        .get_callflow(&self.tenant_uuid, &extension.parent_id)
                        .await
                        .ok_or_else(|| anyhow!("no such callflow"))?;
                    let switchboard = self.switchboard.clone();
                    let _ = tokio::spawn(async move {
                        let _ = switchboard.switch_callflow(&callflow).await;
                    })
                    .await;
                    return Err(anyhow!("no next module"));
                }
                "sipuser" | "huntgroup" => {
                    let bridge_extension = self
                        .format_extension(&json!({
                            "type": extension.discriminator.clone(),
                            "uuid": extension.parent_id.clone(),
                        }))
                        .await
                        .ok_or_else(|| anyhow!("can't format extension"))?;
                    let ringtone = self
                        .module_value(module, "ringtone")
                        .unwrap_or_else(|_| "".to_string());
                    let announcement = self
                        .module_value(module, "announcement")
                        .unwrap_or_else(|_| "".to_string());
                    let ring_timeout = module
                        .get("dial_duration")
                        .and_then(|v| v.as_f64())
                        .map(|v| v as u64)
                        .unwrap_or(60);
                    let bridge = self
                        .switchboard
                        .channel
                        .bridge_extensions(
                            &self.tenant_uuid,
                            vec![&bridge_extension],
                            &ringtone,
                            &announcement,
                            ring_timeout,
                            None,
                            true,
                            false,
                        )
                        .await?;
                    let bridge_result = bridge.wait().await?;
                    let next = match bridge_result {
                        ChannelBridgeResult::Ok => {
                            self.module_value(module, "answer")?
                        }
                        ChannelBridgeResult::Busy => {
                            if let Ok(busy) = self.module_value(module, "busy") {
                                busy
                            } else {
                                self.module_value(module, "noanswer")?
                            }
                        }
                        ChannelBridgeResult::Unavailable => {
                            if let Ok(unavailable) =
                                self.module_value(module, "unavailable")
                            {
                                unavailable
                            } else {
                                self.module_value(module, "noanswer")?
                            }
                        }
                        ChannelBridgeResult::Noanswer
                        | ChannelBridgeResult::NoanswerWithCompletedElsewhere
                        | ChannelBridgeResult::Cancelled => {
                            self.module_value(module, "noanswer")?
                        }
                    };
                    if next == "sipuser_voicemail" {
                        let voicemail_user = SWITCHBOARD_SERVICE
                            .db
                            .get_user_voicemail(&extension.parent_id)
                            .await
                            .ok_or_else(|| anyhow!("user doesn't have voicemail"))?;
                        self.switchboard.voicemail(&voicemail_user, "").await?;
                        return Err(anyhow!("no next module"));
                    }
                    return Ok(next.to_string());
                }
                _ => return Err(anyhow!("no next module")),
            },
        }
    }

    async fn switch_forward_call(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let number = module
            .get("number")
            .ok_or_else(|| anyhow!("can't find number"))?
            .as_str()
            .ok_or_else(|| anyhow!("number is not str"))?;
        if let Ok(exten) = number.parse::<i64>() {
            if let Some(extension) = SWITCHBOARD_SERVICE
                .db
                .get_extension(&self.tenant_uuid, exten)
                .await
            {
                let mut context =
                    self.switchboard.channel.get_call_context().await?;
                context.forward = true;
                self.switchboard.channel.set_call_context(&context).await?;
                self.switchboard.switch_extension(extension, false).await?;
                return Err(anyhow!("no next module"));
            }
        }

        let bridge = self
            .switchboard
            .channel
            .bridge_extensions(
                &self.tenant_uuid,
                vec![&self
                    .format_extension(&serde_json::json!({
                        "type": "forward",
                        "number": number,
                    }))
                    .await
                    .ok_or_else(|| anyhow!("can't format extension"))?],
                "",
                "",
                300,
                None,
                true,
                false,
            )
            .await?;
        let _bridge_result = bridge.wait().await?;

        Err(anyhow!("no next module"))
    }

    async fn switch_backupextension(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let extension = module
            .get("extension")
            .ok_or_else(|| anyhow!("can't find extension"))?
            .as_i64()
            .ok_or_else(|| anyhow!("extension is not number"))?;
        let user: User = serde_json::from_value(
            module
                .get("user")
                .ok_or_else(|| anyhow!("can't find extension"))?
                .clone(),
        )?;
        let mut context = self.switchboard.channel.get_call_context().await?;
        context.backup = Some(user);
        self.switchboard.channel.set_call_context(&context).await?;
        let extension = SWITCHBOARD_SERVICE
            .db
            .get_extension(&self.tenant_uuid, extension)
            .await
            .ok_or_else(|| anyhow!("no extension"))?;
        self.switchboard.switch_extension(extension, false).await?;
        Err(anyhow!("no next module"))
    }

    async fn switch_pressextension(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let dial_by_extension = module
            .get("dial_by_extension")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let sound = self
            .module_value(module, "sound")
            .unwrap_or_else(|_| "".to_string());

        let get_duration =
            || -> Option<u64> { module.get("duration")?.as_f64().map(|v| v as u64) };
        let duration = get_duration().unwrap_or(10) * 1000;

        let digits = self
            .switchboard
            .channel
            .play_sound_and_get_dtmf(
                vec![sound],
                duration,
                5000,
                if !dial_by_extension { 1 } else { 100 },
            )
            .await;

        let get_next = |next: &str| -> Option<&str> {
            module.get("next")?.as_object()?.get(next)?.as_str()
        };
        let get_next_with_error = |next: &str| -> Result<&str> {
            get_next(next).ok_or_else(|| anyhow!("can't find next {next}"))
        };

        if digits.is_empty() {
            if let Some(timeout) = get_next("timeout") {
                return Ok(timeout.to_string());
            }
            return Err(anyhow!("no next module"));
        }

        let digits = digits.join("");

        if let Some(next) = get_next(&digits) {
            return Ok(next.to_string());
        }

        let extension = if let Ok(extension) = digits.parse::<i64>() {
            if dial_by_extension {
                SWITCHBOARD_SERVICE
                    .db
                    .get_extension(&self.tenant_uuid, extension)
                    .await
            } else {
                None
            }
        } else {
            None
        };

        if dial_by_extension {
            if let Some(extension) = extension {
                match extension.discriminator.as_str() {
                    "callflow" => {
                        let callflow = SWITCHBOARD_SERVICE
                            .db
                            .get_callflow(&self.tenant_uuid, &extension.parent_id)
                            .await
                            .ok_or_else(|| anyhow!("no such callflow"))?;
                        let switchboard = self.switchboard.clone();
                        let _ = tokio::spawn(async move {
                            let _ = switchboard.switch_callflow(&callflow).await;
                        })
                        .await;
                        return Err(anyhow!("no next module"));
                    }
                    "sipuser" | "huntgroup" => {
                        let bridge_extension = self
                            .format_extension(&json!({
                                "type": extension.discriminator.clone(),
                                "uuid": extension.parent_id.clone(),
                            }))
                            .await
                            .ok_or_else(|| anyhow!("can't format extension"))?;
                        let ringtone = self
                            .module_value(module, "ringtone")
                            .unwrap_or_else(|_| "".to_string());
                        let announcement = self
                            .module_value(module, "announcement")
                            .unwrap_or_else(|_| "".to_string());
                        let ring_timeout = module
                            .get("dial_duration")
                            .and_then(|v| v.as_f64())
                            .map(|v| v as u64)
                            .unwrap_or(60);
                        let bridge = self
                            .switchboard
                            .channel
                            .bridge_extensions(
                                &self.tenant_uuid,
                                vec![&bridge_extension],
                                &ringtone,
                                &announcement,
                                ring_timeout,
                                None,
                                true,
                                false,
                            )
                            .await?;
                        let bridge_result = bridge.wait().await?;
                        let next = match bridge_result {
                            ChannelBridgeResult::Ok => {
                                get_next_with_error("answer")?
                            }
                            ChannelBridgeResult::Busy => {
                                if let Ok(busy) = get_next_with_error("busy") {
                                    busy
                                } else {
                                    get_next_with_error("noanswer")?
                                }
                            }
                            ChannelBridgeResult::Unavailable => {
                                if let Ok(unavailable) =
                                    get_next_with_error("unavailable")
                                {
                                    unavailable
                                } else {
                                    get_next_with_error("noanswer")?
                                }
                            }
                            ChannelBridgeResult::Noanswer
                            | ChannelBridgeResult::NoanswerWithCompletedElsewhere
                            | ChannelBridgeResult::Cancelled => {
                                get_next_with_error("noanswer")?
                            }
                        };
                        if next == "sipuser_voicemail" {
                            let voicemail_user = SWITCHBOARD_SERVICE
                                .db
                                .get_user_voicemail(&extension.parent_id)
                                .await
                                .ok_or_else(|| {
                                    anyhow!("user doesn't have voicemail")
                                })?;
                            self.switchboard.voicemail(&voicemail_user, "").await?;
                            return Err(anyhow!("no next module"));
                        }
                        return Ok(next.to_string());
                    }
                    _ => {}
                }
                return Err(anyhow!("no next module"));
            }
        }

        if let Some(invalid) = get_next("invalid") {
            return Ok(invalid.to_string());
        }

        Err(anyhow!("no next module"))
    }

    fn ring_timeout(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Option<u64> {
        module.get("duration")?.as_f64().map(|v| v as u64)
    }

    async fn switch_extension(
        &self,
        module: &HashMap<String, serde_json::Value>,
    ) -> Result<String> {
        let extensions = module
            .get("extensions")
            .ok_or_else(|| anyhow!("not extrensions in extension module"))?
            .as_array()
            .ok_or_else(|| anyhow!("extensions in extension module not array"))?;

        let extensions = stream::iter(extensions)
            .filter_map(
                |extension| async move { self.format_extension(extension).await },
            )
            .collect::<Vec<BridgeExtension>>()
            .await;

        let ring_timeout = self.ring_timeout(module).unwrap_or(60);
        let ringtone = self
            .module_value(module, "ringtone")
            .unwrap_or_else(|_| "".to_string());
        let announcement = self
            .module_value(module, "announcement")
            .unwrap_or_else(|_| "".to_string());
        let caller_id = module
            .get("caller_id")
            .and_then(|v| serde_json::from_value::<CallerId>(v.clone()).ok());

        let bridge = self
            .switchboard
            .channel
            .bridge_extensions(
                &self.tenant_uuid,
                extensions.iter().collect(),
                &ringtone,
                &announcement,
                ring_timeout,
                caller_id,
                true,
                false,
            )
            .await?;
        println!("start to wait bridge result");
        let bridge_result = bridge.wait().await?;
        println!("bridge result is {bridge_result:?}");
        let next = match bridge_result {
            ChannelBridgeResult::Ok => self.module_value(module, "answer")?,
            ChannelBridgeResult::Busy => {
                if let Ok(busy) = self.module_value(module, "busy") {
                    busy
                } else {
                    self.module_value(module, "noanswer")?
                }
            }
            ChannelBridgeResult::Unavailable => {
                if let Ok(unavailable) = self.module_value(module, "unavailable") {
                    unavailable
                } else {
                    self.module_value(module, "noanswer")?
                }
            }
            ChannelBridgeResult::Noanswer
            | ChannelBridgeResult::NoanswerWithCompletedElsewhere
            | ChannelBridgeResult::Cancelled => {
                self.module_value(module, "noanswer")?
            }
        };

        Ok(next)
    }
}

/// parse the exten to see if there's operator code
/// in the response
/// The first one is the operator code itself,
/// and wether the exten is only the operator code itself.
///
/// The second one is the exten after striping operator code
/// for exten which is operator code itself, it will be kept
pub fn get_operator_code(
    cc: phonenumber::country::Id,
    exten: &str,
) -> (Option<(&str, bool)>, &str) {
    if cc != phonenumber::country::Id::GB {
        return (None, exten);
    }
    if let Some(text_relay) = exten.strip_prefix("18000") {
        return (Some(("18000", false)), text_relay);
    }
    if let Some(text_relay) = exten.strip_prefix("18001") {
        return (Some(("18001", false)), text_relay);
    }
    if let Some(text_relay) = exten.strip_prefix("18002") {
        return (Some(("18002", false)), text_relay);
    }
    if [
        "116000", "116111", "116123", "116006", "116117", "195", "100", "999",
        "101", "111", "112", "105", "119",
    ]
    .contains(&exten)
    {
        return (Some((exten, true)), exten);
    }
    (None, exten)
}
