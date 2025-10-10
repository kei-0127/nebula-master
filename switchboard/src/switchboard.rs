use crate::callflow::{get_operator_code, OffnetConnect};
use crate::callqueue::CallQueueResult;
use crate::channel::internal_caller_id;
use crate::{bridge::Bridge, channel::AuthPinResult, channel::CallMonitoring};

use super::callflow::CallFlow;
use super::{channel::Channel, server::SWITCHBOARD_SERVICE};
use anyhow::{anyhow, Error, Result};
use chrono::{DateTime, Datelike, Month, NaiveTime, Timelike, Utc, Weekday};
use chrono_tz::{Tz, UTC};
use nebula_db::message::{
    CallContext, CallDirection, ChannelEvent, Endpoint, NewInboundChannelRequest,
    Response, TransferPeer, TransferTarget, UserAgentType,
};
use nebula_db::models::{
    self, is_valid_number, CacheKey, CdrItemChange, DiaryRecord, Extension, Number,
    NumberChange, User, VoicemailMessage, VoicemailUser,
};
use nebula_redis::REDIS;
use nebula_rpc::client::notify_dialog;
use nebula_utils::uuid_v5;
use phonenumber::{self, PhoneNumber};
use regex::Regex;
use serde_json::{self, json, Value};
use std::ops::Add;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use std::{collections::HashMap, time::Duration};
use thiserror::Error;
use tracing::{info, warn};
use yaypi::api::YaypiClient;

const SAVE_RECORDING: &str = "1";
const LISTEN_RECORDING: &str = "2";
const RERECORD: &str = "3";
const EXIT_RECORD: &str = "9";

const VMNEW: &str = "1";
const VMSAVED: &str = "2";
const VMPASSWORD: &str = "3";
const VMRECORDGREETING: &str = "4";
const VMPLAYGREETING: &str = "5";

const VMNEXT: &str = "1";
const VMPREVIOUS: &str = "2";
const VMREPEAT: &str = "3";
const VMDELETE: &str = "5";

const VMSAVERECORDGREETING: &str = "1";
const VMLISTENRECORDGREETING: &str = "2";
const VMRERECORDGREETING: &str = "3";
const VMEXITRECORDGREETING: &str = "9";

const VMMAINMENU: &str = "9";

#[derive(Debug, Error)]
pub enum SwitchboardError {
    #[error("invalid phone number")]
    InvalidPhoneNumber,

    #[error("yaypi create call faliure")]
    YaypiCreateCallFailure,

    #[error("invalid call flow")]
    InvalidCallFlow,

    #[error("invalid diary")]
    InvalidDiary,
}

#[derive(Clone)]
pub struct Switchboard {
    pub channel: Channel,
    pub yaypi_client: Arc<YaypiClient>,
}

impl Switchboard {
    pub fn new(channel: Channel) -> Switchboard {
        let yaypi_client = Arc::new(YaypiClient::new());
        Switchboard {
            // req,
            channel,
            yaypi_client,
        }
    }

    pub async fn switch(&self, req: NewInboundChannelRequest) -> Result<(), Error> {
        if let Some((target, auto_answer, empty_sdp)) = &req.raw_transfer {
            self.channel
                .raw_transfer(target, *auto_answer, *empty_sdp, &req.to)
                .await?;
        }

        if let Some(transfer) = &req.transfer {
            if let TransferPeer::Recipient {
                target: TransferTarget::Location(location, endpoint),
                auto_answer,
                destination,
                ..
            } = &transfer.peer
            {
                let bridge = self
                    .channel
                    .bridge_transfer_location(
                        endpoint,
                        location,
                        *auto_answer,
                        60,
                        destination,
                    )
                    .await?;
                bridge.wait().await?;
                return Ok(());
            }
        }

        if let Some(cx) = &req.teams_refer_cx {
            let bridge = self.channel.bridge_teams_refer(cx, 60).await?;
            bridge.wait().await?;
            return Ok(());
        }

        match &req.from {
            Endpoint::Provider {
                number,
                diversion,
                pai,
                ..
            } => {
                if let Some(inter_tenant_info) = &req.inter_tenant {
                    let _ = self
                        .channel
                        .update_cdr(CdrItemChange {
                            tenant_id: Some(inter_tenant_info.dst_tenant_id.clone()),
                            call_type: Some("inbound".to_string()),
                            from_type: Some("inter_tenant".to_string()),
                            from_exten: Some(inter_tenant_info.from_exten.clone()),
                            from_uuid: Some(inter_tenant_info.src_tenant_id.clone()),
                            from_name: Some(
                                inter_tenant_info.from_display_name.clone(),
                            ),
                            ..Default::default()
                        })
                        .await;
                    self.switch_internal(&inter_tenant_info.dst_tenant_id, &req)
                        .await?;
                    return Ok(());
                }
                if req.callflow.is_none() {
                    let _ = self
                        .channel
                        .update_cdr(CdrItemChange {
                            call_type: Some("inbound".to_string()),
                            from_type: Some("number".to_string()),
                            from_exten: Some(number.to_string()),
                            ..Default::default()
                        })
                        .await;
                }
                if diversion.is_some() || pai.is_some() {
                    let mut context = self.channel.get_call_context().await?;
                    context.diversion = diversion.to_owned();
                    context.pai = pai.to_owned();
                    let _ = self.channel.set_call_context(&context).await;
                }

                self.switch_inbound(&req).await?;
            }
            Endpoint::User { user, .. } => {
                let num_channels = user.num_channels().await.unwrap_or(0);
                if num_channels > 50 {
                    tracing::info!(
                        channel = self.channel.id,
                        "hangup the channel because user {} has {num_channels} channels",
                        user.uuid,
                    );
                    return Ok(());
                };

                let _ = self
                    .channel
                    .update_cdr(CdrItemChange {
                        tenant_id: Some(user.tenant_id.clone()),
                        from_uuid: Some(user.uuid.clone()),
                        from_exten: Some(user.extension.unwrap_or(0).to_string()),
                        from_type: Some("sipuser".to_string()),
                        from_user: Some(user.name.clone()),
                        from_name: Some(
                            user.display_name
                                .clone()
                                .unwrap_or_else(|| "".to_string()),
                        ),
                        ..Default::default()
                    })
                    .await;
                self.switch_internal(&user.tenant_id, &req).await?
            }
            Endpoint::Trunk {
                trunk,
                number,
                extension,
                ziron_tag,
                ..
            } => {
                let _ = self
                    .channel
                    .update_cdr(CdrItemChange {
                        tenant_id: Some(trunk.tenant_id.clone()),
                        from_uuid: Some(trunk.uuid.clone()),
                        from_exten: Some(number.to_string()),
                        from_type: Some("trunk".to_string()),
                        from_name: Some(trunk.name.clone()),
                        from_user: Some(extension.to_string()),
                        ziron_tag: ziron_tag.clone(),
                        ..Default::default()
                    })
                    .await;

                let mut req = req.clone();
                if !["999", "101", "111", "112", "105", "119"]
                    .contains(&req.to.as_str())
                {
                    req.to = get_phone_number(&req.to)
                        .map(|n| {
                            n.format().mode(phonenumber::Mode::E164).to_string()
                        })
                        .unwrap_or(req.to);
                }
                self.switch_internal(&trunk.tenant_id, &req).await?
            }
        }
        Ok(())
    }

    async fn switch_internal(
        &self,
        tenant_id: &str,
        req: &NewInboundChannelRequest,
    ) -> Result<()> {
        println!("start internal");

        if let Ok(contex) = self.channel.get_call_context().await {
            if contex.refer.is_none() {
                if let Some(replaces) = &req.replaces {
                    self.channel
                        .update_recording_from_user(
                            req.from.user(),
                            None,
                            req.refer.is_some(),
                        )
                        .await?;
                    if let Endpoint::User {
                        user_agent_type, ..
                    } = &contex.endpoint
                    {
                        if user_agent_type == &Some(UserAgentType::MsTeams) {
                            let replaces =
                                replaces.split(';').next().unwrap_or("").to_string();
                            self.channel
                                .replace_channel(&Channel::new(replaces))
                                .await?;
                            return Ok(());
                        }
                    }
                    self.channel
                        .pickup_channel(&Channel::new(replaces.to_string()))
                        .await?;
                    return Ok(());
                }
            }
        }

        let mut req = req.clone();
        req.to = urlencoding::decode(&req.to)?.to_string();

        if req.to.starts_with("#") {
            if let Some(speed_dial) = SWITCHBOARD_SERVICE
                .db
                .get_speed_dial(tenant_id, &req.to[1..])
                .await
            {
                req.to = speed_dial.number;
            }
        }

        if !req.to.starts_with('*') {
            let no_star_shortcodes = SWITCHBOARD_SERVICE
                .db
                .get_no_star_shortcodes(tenant_id)
                .await
                .unwrap_or_default();
            if !no_star_shortcodes.is_empty()
                && no_star_shortcodes
                    .iter()
                    .any(|s| req.to.starts_with(&s.shortcode))
            {
                req.to = format!("*{}", req.to);
            }
        }

        let (exten, to_voicemail_message) = match req.to.strip_prefix("**") {
            Some(exten) => (exten.to_string(), true),
            None => (req.to.clone(), false),
        };

        let exten = if exten.starts_with('*') {
            self.channel
                .update_cdr(CdrItemChange {
                    call_type: Some("internal".to_string()),
                    to_type: Some("shortcode".to_string()),
                    to_exten: Some(req.to.clone()),
                    ..Default::default()
                })
                .await?;
            let (continue_call, new_exten) =
                self.switch_shortcode(tenant_id, &req.to).await?;
            if !continue_call {
                return Ok(());
            }
            new_exten
        } else {
            if let Some(from_user) = req.from.user() {
                if from_user.hot_desk_user.unwrap_or(false) {
                    // if it's a hot desk user, we hangup the call
                    return Ok(());
                }
            }
            exten
        };

        if let Some(user) = SWITCHBOARD_SERVICE.db.get_user_by_name(&exten).await {
            if &user.tenant_id == tenant_id {
                self.channel
                    .update_recording_from_user(
                        req.from.user(),
                        Some(&user),
                        req.refer.is_some(),
                    )
                    .await?;
                let extension = Extension {
                    id: 0,
                    number: user.extension.unwrap_or(0),
                    tenant_id: user.tenant_id.clone(),
                    deleted: 0,
                    discriminator: "sipuser".to_string(),
                    parent_id: user.uuid.clone(),
                };
                self.switch_extension(extension, false).await?;
                return Ok(());
            }
        }

        if exten.to_lowercase() == "personal_mailbox"
            || exten.to_lowercase() == "voicemail"
        {
            let context = self.channel.get_call_context().await?;
            let user = context.endpoint.user().ok_or(anyhow!("not from a user"))?;
            let mailbox = user.personal_vmbox.clone().unwrap_or("".to_string());
            let vm_user = SWITCHBOARD_SERVICE
                .db
                .get_voicemail_user(&user.tenant_id, &mailbox)
                .await
                .ok_or(anyhow!("no mailbox found"))?;
            self.switch_voicemail_menu(&context, &vm_user, false)
                .await?;
            return Ok(());
        }

        if exten != "999" {
            if let Ok(exten) = exten.parse::<i64>() {
                if let Some(extension) =
                    SWITCHBOARD_SERVICE.db.get_extension(tenant_id, exten).await
                {
                    self.channel
                        .update_cdr(CdrItemChange {
                            call_type: Some("internal".to_string()),
                            to_type: Some(extension.discriminator.to_string()),
                            to_uuid: Some(extension.parent_id.to_string()),
                            to_exten: Some(extension.number.to_string()),
                            ..Default::default()
                        })
                        .await?;
                    if let Some(from_user) = req.from.user() {
                        let to_user = if extension.discriminator == "sipuser" {
                            let user = SWITCHBOARD_SERVICE
                                .db
                                .get_user(&extension.tenant_id, &extension.parent_id)
                                .await
                                .ok_or(anyhow!("no such sipuser"))?;

                            Some(user)
                        } else {
                            None
                        };

                        self.channel
                            .update_recording_from_user(
                                Some(from_user),
                                to_user.as_ref(),
                                req.refer.is_some(),
                            )
                            .await?;
                    }
                    self.switch_extension(extension, to_voicemail_message)
                        .await?;
                    return Ok(());
                }
            }
        }

        if req.inter_tenant.is_some() {
            return Err(anyhow!(
                "inter tenant dialling didn't find extension number"
            ));
        }

        self.channel
            .update_recording_from_user(req.from.user(), None, req.refer.is_some())
            .await?;
        self.switch_number(tenant_id, &exten).await?;
        Ok(())
    }

    async fn switch_shortcode(
        &self,
        tenant_id: &str,
        exten: &str,
    ) -> Result<(bool, String)> {
        let shortcodes = SWITCHBOARD_SERVICE
            .db
            .get_shortcodes(tenant_id)
            .await
            .ok_or(anyhow!("no shortcodes in the account"))?;
        for shortcode in shortcodes {
            if exten[1..].starts_with(&shortcode.shortcode) {
                let value: Value = serde_json::from_str(&shortcode.feature)?;
                let feature = value
                    .get("feature")
                    .ok_or(anyhow!("shortcode doesn't have feature"))?
                    .as_str()
                    .ok_or(anyhow!("shortcode feature is not string"))?;
                if let Ok(context) = self.channel.get_call_context().await {
                    if let Some(from_user) = context.endpoint.user() {
                        if from_user.hot_desk_user.unwrap_or(false)
                            && !feature.to_lowercase().starts_with("hotdesk")
                        {
                            // if it's a hot desk user, we hangup the call
                            return Err(anyhow!(
                                "hot desk user can only dial hotdesk shortcode"
                            ));
                        }
                    }
                }
                let continue_call = match feature.to_lowercase().as_str() {
                    "setcallerid" => {
                        let mut context = self.channel.get_call_context().await?;
                        context.set_caller_id = Some(
                            value
                                .get("callerid")
                                .ok_or(anyhow!(
                                    "shortcode setcallerid doesn't have callerid"
                                ))?
                                .as_str()
                                .ok_or(anyhow!("callerid is not string"))?
                                .to_string(),
                        );
                        self.channel.set_call_context(&context).await?;
                        true
                    }
                    "parkretrieve" => {
                        let slot = value.get("slot").ok_or_else(|| {
                            anyhow!("shortcode doesn't have feature")
                        })?;
                        let slot = format!("{}_{}", slot, shortcode.tenant_id);
                        let mut context = self.channel.get_call_context().await?;
                        if let Some(refer) = context.refer.as_ref() {
                            let peer = Channel::new(refer.peer.to_string());
                            let park_uuid = peer.park(slot.clone(), true).await?;
                            let _ = peer.publish_park(park_uuid, slot, true).await;
                            context.refer = None;
                            self.channel.set_call_context(&context).await?;
                        } else {
                            info!(
                                channel = self.channel.id,
                                "[{}] start to retrieve slot {slot}",
                                self.channel.id
                            );
                            if let Err(e) = self.channel.retrieve(slot).await {
                                warn!(
                                    channel = self.channel.id,
                                    "retrieve a call error: {e}"
                                );
                            }
                        }
                        false
                    }
                    "globalpickup" => {
                        let context = self.channel.get_call_context().await?;
                        let user = context
                            .endpoint
                            .user()
                            .ok_or_else(|| anyhow!("not coming from a user"))?;
                        if !user.pickup.unwrap_or(false) {
                            return Err(anyhow!("user isn't allowed to pickup"));
                        }
                        info!(
                            channel = self.channel.id,
                            "[{}] now try global pickup", &self.channel.id
                        );
                        let key = &format!(
                            "nebula:tenant:{}:incoming_bridge",
                            &user.tenant_id
                        );
                        let bridges: Vec<String> = REDIS.lrange(key, 0, -1).await?;
                        info!(
                            channel = self.channel.id,
                            "[{}] global pickup has {} bridges available",
                            self.channel.id,
                            bridges.len()
                        );
                        for bridge_id in bridges {
                            let bridge = Bridge::new(bridge_id.clone());
                            if !bridge.is_hangup().await.unwrap_or(true)
                                && !bridge.answered().await
                            {
                                bridge.add_channel(&self.channel.id).await?;

                                let inbound = bridge
                                    .get_inbound()
                                    .await
                                    .unwrap_or(Channel::new("".to_string()));
                                if let Ok(inbound_context) =
                                    inbound.get_call_context().await
                                {
                                    let mut context =
                                        self.channel.get_call_context().await?;
                                    context.answer_caller_id = Some(
                                        internal_caller_id(
                                            &inbound_context,
                                            user.cc.clone(),
                                            None,
                                        )
                                        .await,
                                    );
                                    self.channel.set_call_context(&context).await?;
                                }

                                self.channel.answer().await?;
                                bridge.answer(&self.channel).await?;
                                self.channel
                                    .receive_event("0", ChannelEvent::Hangup)
                                    .await?;
                                break;
                            } else {
                                info!(
                            channel = self.channel.id,
                                    "[{}] global pickup bridge {} was hangup or answered, remove now", &self.channel.id, &bridge_id);
                                let _ = REDIS.lrem(key, 0, &bridge_id).await;
                            }
                        }
                        info!(
                            channel = self.channel.id,
                            "[{}] global pickup stop", &self.channel.id
                        );
                        false
                    }
                    "pickup" => {
                        let context = self.channel.get_call_context().await?;
                        let from_user = context
                            .endpoint
                            .user()
                            .ok_or_else(|| anyhow!("not coming from a user"))?;

                        let exten = exten[shortcode.shortcode.len() + 1..]
                            .to_string()
                            .parse::<i64>()?;
                        let extension = SWITCHBOARD_SERVICE
                            .db
                            .get_extension(tenant_id, exten)
                            .await
                            .ok_or_else(|| anyhow!("no extension"))?;

                        match extension.discriminator.as_str() {
                            "sipuser" => {
                                let to_user = SWITCHBOARD_SERVICE
                                    .db
                                    .get_user(tenant_id, &extension.parent_id)
                                    .await
                                    .ok_or_else(|| anyhow!("can'd find sipuser"))?;
                                self.channel
                                    .pickup_user(from_user, &to_user)
                                    .await?;
                            }
                            "huntgroup" => {
                                let users = SWITCHBOARD_SERVICE
                                    .db
                                    .get_group_users(tenant_id, &extension.parent_id)
                                    .await
                                    .ok_or_else(|| {
                                        anyhow!("can't get group users")
                                    })?;
                                for to_user in users {
                                    if self
                                        .channel
                                        .pickup_user(&from_user, &to_user)
                                        .await
                                        .is_ok()
                                    {
                                        break;
                                    }
                                }
                            }
                            _ => {
                                return Err(anyhow!(
                                    "can only pickup a sipuser or huntgroup"
                                ));
                            }
                        }
                        false
                    }
                    "pickupqueue" => {
                        let queue = value
                            .get("queue")
                            .ok_or_else(|| {
                                anyhow!("shortcode doesn't have feature")
                            })?
                            .as_str()
                            .ok_or_else(|| anyhow!("queue isn't str"))?;
                        let queue = SWITCHBOARD_SERVICE
                            .db
                            .get_queue_group(queue)
                            .await
                            .ok_or_else(|| anyhow!("can't find queue"))?;
                        if queue.tenant_id != shortcode.tenant_id {
                            return Err(anyhow!("don't have queue in the account"));
                        }
                        let key = &format!("nebula:callqueue:{}:calls", &queue.uuid);
                        let channels = REDIS.zrange(key, 0, 0).await?;
                        let first_channel = channels
                            .first()
                            .ok_or_else(|| anyhow!("no channel in queue"))?;
                        let first_channel = Channel::new(first_channel.to_string());
                        let bridge_id = nebula_utils::uuid();
                        REDIS
                            .hset(
                                &first_channel.redis_key(),
                                "bridge_id",
                                &bridge_id,
                            )
                            .await?;
                        let bridge = Bridge::new(bridge_id);
                        bridge.set_inbound(&first_channel.id).await?;
                        bridge.set_tenant_id(&shortcode.tenant_id).await?;
                        bridge.add_channel(&self.channel.id).await?;
                        self.channel.answer().await?;
                        bridge.answer(&self.channel).await?;
                        first_channel
                            .new_event(
                                ChannelEvent::CallQueueWaitDone,
                                &CallQueueResult::Ok.to_string(),
                            )
                            .await?;
                        self.channel
                            .receive_event("0", ChannelEvent::Hangup)
                            .await?;
                        false
                    }
                    "togglegroupmember" => {
                        self.channel
                            .toggle_group_member(
                                &exten[shortcode.shortcode.len() + 1..],
                            )
                            .await?;
                        false
                    }
                    "togglequeueavailable" => {
                        let context = self.channel.get_call_context().await?;
                        let shortcode_user =
                            shortcode_user(tenant_id, &shortcode.shortcode, exten)
                                .await;
                        let user = if let Some(user) = shortcode_user.as_ref() {
                            user
                        } else {
                            context
                                .endpoint
                                .user()
                                .ok_or_else(|| anyhow!("not coming from a user"))?
                        };
                        let available = !user.is_available().await.unwrap_or(false);

                        let user_uuid = user.uuid.clone();
                        tokio::spawn(async move {
                            SWITCHBOARD_SERVICE
                                .db
                                .update_user_available(user_uuid, available)
                                .await;
                        });

                        for key in user.cache_keys() {
                            let _ = REDIS.del(&key).await;
                        }
                        let _ = notify_dialog(
                            SWITCHBOARD_SERVICE
                                .config
                                .all_proxies()
                                .map(|p| p.to_vec())
                                .unwrap_or_else(|| {
                                    vec![SWITCHBOARD_SERVICE
                                        .config
                                        .proxy_host
                                        .clone()]
                                }),
                            format!("{}:queue_availability", user.uuid),
                        )
                        .await;
                        self.channel.answer().await?;
                        tokio::time::sleep(Duration::from_millis(500)).await;

                        let beep = value
                            .get("beep")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false);

                        if beep {
                            if available {
                                self.channel
                                    .play_sound(
                                        None,
                                        vec!["5ca619c1-7b2e-4642-be10-89efb6498281"
                                            .to_string()],
                                        true,
                                    )
                                    .await?;
                            } else {
                                self.channel
                                    .play_sound(
                                        None,
                                        vec!["44a1aa30-a6b5-4b79-a319-d2e56e4c97c1"
                                            .to_string()],
                                        true,
                                    )
                                    .await?;
                            }
                        } else {
                            let tts_text = format!(
                                "tts://{} is now set to {} available",
                                if let Some(user) = shortcode_user.as_ref() {
                                    "Extension".to_string()
                                        + &user.extension.unwrap_or(0).to_string()
                                } else {
                                    "Your sip user".to_string()
                                },
                                if available { "" } else { "not" }
                            );
                            self.channel
                                .play_sound(None, vec![tts_text], true)
                                .await?;
                        }

                        false
                    }
                    "togglequeuelogin" => {
                        let context = self.channel.get_call_context().await?;
                        let from_user = context
                            .endpoint
                            .user()
                            .ok_or_else(|| anyhow!("not coming from a user"))?;
                        let shortcode_user =
                            shortcode_user(tenant_id, &shortcode.shortcode, exten)
                                .await;
                        let to_user = if let Some(user) = shortcode_user.as_ref() {
                            user
                        } else {
                            from_user
                        };

                        let check_for_self =
                            if let Some(val) = value.get("check_for_self") {
                                val.as_bool().ok_or_else(|| {
                                    anyhow!("Inavlid check_for_self, should be bool")
                                })?
                            } else {
                                false
                            };

                        self.channel
                            .toggle_queue_login(from_user, to_user, check_for_self)
                            .await?;

                        false
                    }
                    "setauxcode" => {
                        let code = value
                            .get("code")
                            .ok_or_else(|| {
                                anyhow!("shortcode doesn't have feature")
                            })?
                            .as_str()
                            .ok_or_else(|| anyhow!("code isn't str"))?;
                        let context = self.channel.get_call_context().await?;
                        let shortcode_user =
                            shortcode_user(tenant_id, &shortcode.shortcode, exten)
                                .await;
                        let from_user = context
                            .endpoint
                            .user()
                            .ok_or_else(|| anyhow!("not coming from a user"))?;
                        let user = if let Some(user) = shortcode_user.as_ref() {
                            user
                        } else {
                            from_user
                        };
                        // check current aux code so that it can fill in the scheduled ones
                        // before this manually set code
                        user.current_aux_code().await;
                        SWITCHBOARD_SERVICE
                            .db
                            .insert_sipuser_aux_log(
                                user.uuid.clone(),
                                Utc::now(),
                                code.to_string(),
                                format!("user:{}", from_user.uuid),
                            )
                            .await?;

                        // now clear cache
                        let _ = reqwest::Client::new()
                            .put(format!(
                                "http://{}/clear_aux_log_cache/{}",
                                SWITCHBOARD_SERVICE.config.proxy_host, user.uuid
                            ))
                            .send()
                            .await;

                        let _ = notify_dialog(
                            SWITCHBOARD_SERVICE
                                .config
                                .all_proxies()
                                .map(|p| p.to_vec())
                                .unwrap_or_else(|| {
                                    vec![SWITCHBOARD_SERVICE
                                        .config
                                        .proxy_host
                                        .clone()]
                                }),
                            user.uuid.clone(),
                        )
                        .await;

                        self.channel.answer().await?;
                        let tts_text = format!(
                            "tts://{} is now set to {code}",
                            if let Some(user) = shortcode_user.as_ref() {
                                "Extension".to_string()
                                    + &user.extension.unwrap_or(0).to_string()
                            } else {
                                "Your sip user".to_string()
                            },
                        );
                        self.channel.play_sound(None, vec![tts_text], true).await?;

                        false
                    }
                    "listen" => {
                        let exten = exten[shortcode.shortcode.len() + 1..]
                            .to_string()
                            .parse::<i64>()?;
                        self.channel
                            .monitor_call(tenant_id, CallMonitoring::Listen, exten)
                            .await?;
                        false
                    }
                    "whisper" => {
                        let exten = exten[shortcode.shortcode.len() + 1..]
                            .to_string()
                            .parse::<i64>()?;

                        self.channel
                            .monitor_call(tenant_id, CallMonitoring::Whisper, exten)
                            .await?;
                        false
                    }
                    "barge" => {
                        let exten = exten[shortcode.shortcode.len() + 1..]
                            .to_string()
                            .parse::<i64>()?;
                        self.channel
                            .monitor_call(tenant_id, CallMonitoring::Barge, exten)
                            .await?;
                        false
                    }
                    "removecallrestrictions" => {
                        let mut context = self.channel.get_call_context().await?;
                        context.remove_call_restrictions = true;
                        self.channel.set_call_context(&context).await?;
                        true
                    }
                    "assigncallroute" => {
                        let context = self.channel.get_call_context().await?;
                        if let Err(err) = self
                            .assign_call_route(&context.endpoint, tenant_id, &value)
                            .await
                        {
                            println!("assign call route error {}", err);
                            self.channel
                                .play_sound(
                                    None,
                                    vec!["9cc49088-d7f9-457b-8995-bec371fda6da"
                                        .to_string()],
                                    true,
                                )
                                .await?;
                        }
                        false
                    }
                    "record" => {
                        self.channel.answer().await?;
                        let name = value
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "Recorded by phone".to_string());
                        let mut first_play = true;
                        let mut play_sound = false;
                        let mut duration = 0.0;
                        let sound_uuid = nebula_utils::uuid();
                        loop {
                            if first_play {
                                first_play = false;
                                let record_text = "tts://record your message at the tone, press any key to end the recording".to_string();
                                self.channel
                                    .play_sound(
                                        None,
                                        vec![
                                            record_text,
                                            "tone://200,0,500,0".to_string(),
                                        ],
                                        true,
                                    )
                                    .await?;
                                let start = SystemTime::now();
                                self.channel
                                    .record_to_file(sound_uuid.clone())
                                    .await?;
                                tokio::select! {
                                    _ = self.channel.receive_event("$", ChannelEvent::Dtmf) => {

                                    }
                                    _ = self.channel.receive_event("0", ChannelEvent::Hangup) => {
                                        break;
                                    }
                                }
                                duration =
                                    start.elapsed()?.as_millis() as f64 / 1000.0;
                                self.channel
                                    .stop_record_to_file(sound_uuid.clone())
                                    .await?;
                            }

                            let mut sounds = Vec::new();
                            if play_sound {
                                play_sound = false;
                                sounds.push(
                                    "record_file://".to_string()
                                        + sound_uuid.as_str(),
                                );
                            }
                            let menu = format!(
                                r#"tts:// Press {} to save the recording. Press {} to listen to the recording. Press {} to rerecord. Press {} to exit."#,
                                SAVE_RECORDING,
                                LISTEN_RECORDING,
                                RERECORD,
                                EXIT_RECORD,
                            );
                            sounds.push(menu);

                            let digits = self
                                .channel
                                .play_sound_and_get_dtmf(sounds, 5000, 5000, 1)
                                .await;
                            let pressed = digits.join("");
                            match pressed.as_str() {
                                SAVE_RECORDING => {
                                    self.channel
                                        .upload_record_to_file(sound_uuid.clone())
                                        .await?;

                                    let tenant_id = tenant_id.to_string();
                                    tokio::spawn(async move {
                                        let _ = SWITCHBOARD_SERVICE
                                            .db
                                            .create_sound(
                                                sound_uuid.clone(),
                                                tenant_id.clone(),
                                                name.clone(),
                                                duration,
                                            )
                                            .await;
                                        let _ = SWITCHBOARD_SERVICE
                                            .yaypi_client
                                            .create_sound(
                                                &sound_uuid,
                                                &tenant_id,
                                                &name,
                                            )
                                            .await;
                                    });
                                    self.channel
                                        .play_sound(
                                            None,
                                            vec!["tts://recording saved".to_string()],
                                            true,
                                        )
                                        .await?;
                                    break;
                                }
                                LISTEN_RECORDING => {
                                    play_sound = true;
                                }
                                RERECORD => {
                                    first_play = true;
                                }
                                EXIT_RECORD => break,
                                _ => (),
                            }

                            if self.channel.is_hangup().await {
                                break;
                            }
                        }
                        false
                    }
                    "loginasuser" => {
                        let exten = exten[shortcode.shortcode.len() + 1..]
                            .to_string()
                            .parse::<i64>()?;
                        self.channel.login_as_user(exten).await?;
                        false
                    }
                    "logoutall" => {
                        self.channel.log_out_all().await?;
                        false
                    }
                    "hotdesk" => {
                        let s = &exten[shortcode.shortcode.len() + 1..];
                        self.channel.hot_desk(s).await?;
                        false
                    }
                    "hotdesklogout" => {
                        let s = &exten[shortcode.shortcode.len() + 1..];
                        self.channel.hot_desk_logout(s).await?;
                        false
                    }
                    "paging" => {
                        let s = &exten[shortcode.shortcode.len() + 1..];
                        self.channel.paging(s).await?;
                        false
                    }
                    "lastcall" => {
                        if exten[1..] == shortcode.shortcode {
                            self.channel.check_last_call().await?;
                            return Ok((false, "".to_string()));
                        } else {
                            return Ok((true, exten[1..].to_string()));
                        }
                    }
                    "setforward" => {
                        let context = self.channel.get_call_context().await?;
                        let from_user = context
                            .endpoint
                            .user()
                            .ok_or_else(|| anyhow!("not coming from a user"))?;
                        let exten = &exten[shortcode.shortcode.len() + 1..];
                        update_user_call_forward(from_user, exten).await;

                        self.channel.answer().await?;
                        self.channel
                            .play_sound(
                                None,
                                vec![format!(
                                    "tts://{}",
                                    if exten.is_empty() {
                                        "call forwarding is cleared".to_string()
                                    } else {
                                        format!("call forwarding is set to <say-as interpret-as=\"telephone\">{exten}</say-as>")
                                    }
                                )],
                                true,
                            )
                            .await?;

                        false
                    }
                    "voicemailmenu" => {
                        let context = self.channel.get_call_context().await?;
                        let user = context
                            .endpoint
                            .user()
                            .ok_or(anyhow!("not from a user"))?;
                        let mailbox =
                            user.personal_vmbox.clone().unwrap_or("".to_string());
                        let vm_user = SWITCHBOARD_SERVICE
                            .db
                            .get_voicemail_user(&user.tenant_id, &mailbox)
                            .await
                            .ok_or(anyhow!("no mailbox found"))?;
                        self.switch_voicemail_menu(&context, &vm_user, false)
                            .await?;
                        false
                    }
                    _ => return Err(anyhow!("feature {} not supported", feature)),
                };
                if !continue_call {
                    return Ok((false, "".to_string()));
                }
                return Ok((
                    true,
                    exten[shortcode.shortcode.len() + 1..].to_string(),
                ));
            }
        }
        Err(anyhow!("doesn't have match shortcode"))
    }

    async fn assign_call_route(
        &self,
        from_endpoint: &Endpoint,
        tenant_id: &str,
        value: &Value,
    ) -> Result<()> {
        let mut from_user = None;
        let mut from_trunk = None;
        match from_endpoint {
            Endpoint::User { user, .. } => {
                from_user = Some(user.uuid.to_string());
            }
            Endpoint::Trunk { trunk, .. } => {
                from_trunk = Some(trunk.uuid.to_string());
            }
            _ => {
                return Err(anyhow!("only user or trunk can assign call route"));
            }
        };

        let number = value
            .get("number")
            .ok_or_else(|| anyhow!("don't have number"))?
            .as_str()
            .ok_or_else(|| anyhow!("number isn't string"))?;
        let number = SWITCHBOARD_SERVICE
            .db
            .get_number_by_uuid(number)
            .await
            .ok_or_else(|| anyhow!("don't have number with this uuid"))?;

        let mut number_change = NumberChange {
            ..Default::default()
        };
        let mut text = "".to_string();
        if let Some(call_route) = value.get("call_route").and_then(|c| c.as_str()) {
            if !call_route.is_empty() {
                let callflow = SWITCHBOARD_SERVICE
                    .db
                    .get_callflow(tenant_id, call_route)
                    .await
                    .ok_or_else(|| anyhow!("invalid callroute"))?;
                text += "Call route ";
                text += callflow.name.as_str();
            }
            number_change.call_flow_id = Some(Some(call_route.to_string()));
        }
        if let Some(diary_uuid) = value.get("diary").and_then(|c| c.as_str()) {
            if !diary_uuid.is_empty() {
                let diary = SWITCHBOARD_SERVICE
                    .db
                    .get_diary(tenant_id, diary_uuid)
                    .await
                    .ok_or(anyhow!("invalid diary"))?;
                if !text.is_empty() {
                    text += " and ";
                }
                text += "time diary ";
                text += diary.name.as_str();
            }
            number_change.diary_id = Some(Some(diary_uuid.to_string()));
        }
        if number_change.call_flow_id.is_none() && number_change.diary_id.is_none() {
            return Err(anyhow!("haven't got change for call flow or diary"));
        }
        let number = SWITCHBOARD_SERVICE
            .db
            .update_number(number.id, number_change)
            .await?;

        let local_number = number.clone();
        tokio::spawn(async move {
            let _ = SWITCHBOARD_SERVICE
                .yaypi_client
                .assign_call_route(
                    from_user.as_deref(),
                    from_trunk.as_deref(),
                    local_number.uuid.as_str(),
                    local_number
                        .call_flow_id
                        .unwrap_or_else(|| "".to_string())
                        .as_str(),
                    local_number
                        .diary_id
                        .unwrap_or_else(|| "".to_string())
                        .as_str(),
                )
                .await;
        });

        text += " set on 0";
        text += number.number.as_str();
        self.channel
            .play_sound(None, vec!["tts://".to_string() + text.as_str()], true)
            .await?;
        Ok(())
    }

    async fn switch_inbound(&self, req: &NewInboundChannelRequest) -> Result<()> {
        if let Some((tenant_id, callflow)) = &req.callflow {
            let flow = serde_json::from_str(callflow)?;
            self.callflow(tenant_id, "".to_string(), flow, false)
                .await?;
            return Ok(());
        }
        if let Some(user) = SWITCHBOARD_SERVICE.db.get_user_by_name(&req.to).await {
            if !user.require_auth {
                self.switch_user(&user.tenant_id, &user.uuid, false).await?;
                return Ok(());
            }
        }

        if let Some(internal_number) = get_internal_number(&req.to).await {
            if let Some(blacklist) = SWITCHBOARD_SERVICE
                .db
                .get_black_list(
                    &internal_number
                        .tenant_id
                        .clone()
                        .unwrap_or_else(|| "".to_string()),
                )
                .await
            {
                if let Ok(patterns) = serde_json::from_str::<Vec<String>>(
                    &blacklist.patterns.unwrap_or_else(|| "".to_string()),
                ) {
                    if self
                        .callerid_match_patterns(patterns)
                        .await
                        .unwrap_or(false)
                    {
                        return Ok(());
                    }
                }
            }

            let mut context = self.channel.get_call_context().await?;
            let e164 = format!("+{}", &internal_number.e164);
            if context.to != e164 {
                context.to = e164;
            }
            context.ziron_tag = internal_number.ziron_tag.clone();
            context.to_internal_number = Some(internal_number.clone());
            let _ = self.channel.set_call_context(&context).await;

            if let Some(cdr) = context.cdr.as_ref() {
                let cdr = *cdr;
                let internal_number = internal_number.clone();
                tokio::spawn(async move {
                    let _ = SWITCHBOARD_SERVICE
                        .yaypi_client
                        .notify_incoming(
                            &cdr.to_string(),
                            &internal_number
                                .tenant_id
                                .clone()
                                .unwrap_or_else(|| "".to_string()),
                            &context.endpoint.extension(),
                            &internal_number.e164,
                        )
                        .await;
                });
            }

            if internal_number.limit == 0 {
                // cut call off if limit is 0
                return Ok(());
            }

            if internal_number.limit > 0 {
                let key =
                    format!("nebula:number:inbound_limit:{}", internal_number.e164);
                let _ = REDIS.setexnx(&key, "0", 86400).await;
                if let Some(tenant_id) = internal_number.tenant_id.as_ref() {
                    let _ = REDIS
                        .setex(
                            &format!("nebula:inbound_limit_for_tenant:{tenant_id}"),
                            86400,
                            "yes",
                        )
                        .await;
                }
                let millis = REDIS.incr_by(&key, 0).await.unwrap_or(0);
                if millis >= internal_number.limit * 1000 {
                    self.channel
                        .notify_inbound_limit(&internal_number, millis)
                        .await;
                    self.switch_internal_number(&Number {
                        id: 0,
                        uuid: "".to_string(),
                        tenant_id: Some("".to_string()),
                        number: "".to_string(),
                        country_code: "".to_string(),
                        e164: req.to.clone(),
                        deleted: 0,
                        call_flow_id: None,
                        diary_id: None,
                        require_auth: false,
                        limit: -1,
                        sip_user_id: None,
                        is_reseller: false,
                        ziron_tag: None,
                        ddi_display_name_show_number: None,
                    })
                    .await?;
                    return Ok(());
                }
            }

            self.switch_internal_number(&internal_number).await?;
        } else {
            self.switch_internal_number(&Number {
                id: 0,
                uuid: "".to_string(),
                tenant_id: Some("".to_string()),
                number: "".to_string(),
                country_code: "".to_string(),
                e164: req.to.clone(),
                deleted: 0,
                call_flow_id: None,
                diary_id: None,
                require_auth: false,
                limit: -1,
                sip_user_id: None,
                is_reseller: false,
                ziron_tag: None,
                ddi_display_name_show_number: None,
            })
            .await?;
        }
        Ok(())
    }

    async fn switch_internal_number(&self, number: &Number) -> Result<(), Error> {
        if number.tenant_id.clone().unwrap_or_else(|| "".to_string()) == "" {
            let context = self.channel.get_call_context().await?;

            let answer_sdp = if let Ok(sdp) = self.channel.get_local_sdp().await {
                sdp
            } else {
                self.channel.create_answer_sdp(false).await?
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
                        self.channel.id.clone(),
                        183,
                        "Session Progress".to_string(),
                        answer_sdp.to_string(),
                    )
                    .await?;
            }
            Channel::play_sound_to_channel(
                &self.channel.id,
                None,
                vec!["tone://375,375,400,400".to_string()],
                40,
                true,
            )
            .await?;

            return Ok(());
        }
        let sip_user_id = number.sip_user_id.clone().unwrap_or("".to_string());
        if !sip_user_id.is_empty() {
            self.channel
                .update_cdr(CdrItemChange {
                    tenant_id: Some(
                        number.tenant_id.clone().unwrap_or("".to_string()),
                    ),
                    to_exten: Some(number.e164.clone()),
                    to_type: Some("number".to_string()),
                    ..Default::default()
                })
                .await?;

            let mut context = self.channel.get_call_context().await?;
            if number.ddi_display_name_show_number.unwrap_or(false) {
                context.call_flow_name = None;
            } else {
                context.call_flow_name = Some("DDI".to_string());
            }

            if let Some(user) = SWITCHBOARD_SERVICE
                .db
                .get_user(number.tenant_id.as_deref().unwrap_or(""), &sip_user_id)
                .await
            {
                if let Some(call_handling) = user.call_handling() {
                    if call_handling.hide_callflow_name.unwrap_or(false) {
                        context.call_flow_name = None;
                    }
                }
            }

            self.channel.set_call_context(&context).await?;
            self.switch_user(
                &number.tenant_id.clone().unwrap_or("".to_string()),
                &sip_user_id,
                false,
            )
            .await?;
            return Ok(());
        }

        self.channel
            .update_cdr(CdrItemChange {
                tenant_id: Some(number.tenant_id.clone().unwrap_or("".to_string())),
                to_exten: Some(number.e164.clone()),
                to_type: Some("number".to_string()),
                ..Default::default()
            })
            .await?;

        if number.diary_id.as_ref().unwrap_or(&"".to_string()) != "" {
            let diary = SWITCHBOARD_SERVICE
                .db
                .get_diary(
                    &number.tenant_id.clone().unwrap_or("".to_string()),
                    number.diary_id.as_ref().unwrap_or(&"".to_string()),
                )
                .await
                .ok_or(SwitchboardError::InvalidDiary)?;
            let records: Vec<DiaryRecord> = serde_json::from_str(&diary.records)?;
            let ctime = now(if diary.timezone != "" {
                Some(&diary.timezone)
            } else {
                None
            });

            for record in records {
                if check_time(&ctime, &record.time).unwrap_or(false) {
                    let callflow = SWITCHBOARD_SERVICE
                        .db
                        .get_callflow(
                            &number.tenant_id.clone().unwrap_or("".to_string()),
                            &record.call_flow,
                        )
                        .await
                        .ok_or(SwitchboardError::InvalidCallFlow)?;
                    self.channel
                        .update_cdr(CdrItemChange {
                            callflow_uuid: Some(callflow.uuid.clone()),
                            callflow_name: Some(callflow.name.clone()),
                            ..Default::default()
                        })
                        .await?;
                    self.switch_callflow(&callflow).await?;
                    return Ok(());
                }
            }
            return Ok(());
        }

        let callflow = SWITCHBOARD_SERVICE
            .db
            .get_callflow(
                &number.tenant_id.clone().unwrap_or("".to_string()),
                number.call_flow_id.as_ref().unwrap_or(&"".to_string()),
            )
            .await
            .ok_or(SwitchboardError::InvalidCallFlow)?;

        self.channel
            .update_cdr(CdrItemChange {
                callflow_uuid: Some(callflow.uuid.clone()),
                callflow_name: Some(callflow.name.clone()),
                ..Default::default()
            })
            .await?;
        self.switch_callflow(&callflow).await?;
        Ok(())
    }

    pub async fn switch_callflow(&self, callflow: &models::CallFlow) -> Result<()> {
        let counter = REDIS
            .hincrby_key_exits(
                &format!("nebula:channel:{}", &self.channel.id),
                &format!("callflow:{}", callflow.uuid),
            )
            .await?;
        if counter > 100 {
            // Callflow loop detected, exit
            return Ok(());
        }

        self.channel
            .update_recording(
                &callflow.tenant_id,
                false,
                callflow.unlimited_recording,
            )
            .await?;
        let mut context = self.channel.get_call_context().await?;
        context.call_flow_id = Some(callflow.uuid.clone());
        context.parent_call_flow_name = context.call_flow_name.clone();
        context.parent_call_flow_show_original_callerid =
            context.call_flow_show_original_callerid.clone();
        context.parent_call_flow_custom_callerid =
            context.call_flow_custom_callerid.clone();
        context.parent_call_flow_moh = context.call_flow_moh.clone();
        if callflow.shown_in_callerid {
            context.call_flow_name = Some(callflow.name.clone());
        } else {
            context.call_flow_name = None;
        }
        context.call_flow_show_original_callerid =
            Some(callflow.show_original_callerid);
        context.call_flow_custom_callerid = callflow.custom_callerid.clone();
        self.channel.set_call_context(&context).await?;

        let flow = serde_json::from_str(&callflow.flow)?;
        self.callflow(
            &callflow.tenant_id,
            callflow.uuid.clone(),
            flow,
            callflow.unlimited_recording,
        )
        .await?;

        let mut context = self.channel.get_call_context().await?;
        context.call_flow_name = context.parent_call_flow_name.clone();
        context.call_flow_show_original_callerid =
            context.parent_call_flow_show_original_callerid.clone();
        context.call_flow_moh = context.parent_call_flow_moh.clone();
        context.call_flow_custom_callerid =
            context.parent_call_flow_custom_callerid.clone();

        Ok(())
    }

    async fn switch_number(
        &self,
        tenant_id: &str,
        number: &str,
    ) -> Result<(), Error> {
        if let Some(tenant_extensions) = SWITCHBOARD_SERVICE
            .db
            .get_tenant_extensions(tenant_id)
            .await
        {
            for tenant_extesion in tenant_extensions {
                if let Some(actual_extension) =
                    number.strip_prefix(&tenant_extesion.number.to_string())
                {
                    let context = self.channel.get_call_context().await?;
                    let user = context
                        .endpoint
                        .user()
                        .ok_or_else(|| anyhow!("only user can dial inter tenant"))?;
                    let user_exten = user.extension.ok_or_else(|| {
                        anyhow!("user doesn't have extension number")
                    })?;
                    self.channel
                        .update_cdr(CdrItemChange {
                            call_type: Some("outbound".to_string()),
                            to_type: Some("inter_tenant".to_string()),
                            to_exten: Some(actual_extension.to_string()),
                            to_uuid: Some(tenant_extesion.dst_tenant_id.clone()),
                            ..Default::default()
                        })
                        .await?;
                    let flow_id = self.channel.id.clone();
                    let flow = serde_json::from_value(serde_json::json!({
                        "start": {
                            "module": "start",
                            "next": "extension"
                        },
                        "extension": {
                            "module": "extension",
                            "extensions": [
                                {
                                    "type": "inter_tenant",
                                    "src_tenant_id": tenant_id,
                                    "dst_tenant_id": tenant_extesion.dst_tenant_id,
                                    "exten": actual_extension,
                                    "from_exten": user_exten.to_string(),
                                    "from_display_name": user.display_name.clone().unwrap_or_default(),
                                }
                            ]
                        }
                    }))?;
                    self.callflow(tenant_id, flow_id, flow, false).await?;
                    return Ok(());
                }
            }
        }

        let context = self.channel.get_call_context().await?;
        let cc = phonenumber::country::Id::from_str(&context.endpoint.cc())
            .unwrap_or(phonenumber::country::GB);
        let (operator_code, parsed_number) = get_operator_code(cc, number);
        let phone_number = phonenumber::parse(Some(cc), parsed_number)?;
        let e164 = if cc == phonenumber::country::AU
            && (number == "000" || number == "+61000")
        {
            "+61000".to_string()
        } else {
            phone_number
                .format()
                .mode(phonenumber::Mode::E164)
                .to_string()
        };
        if operator_code
            .as_ref()
            .map(|(_, short_operator_code)| *short_operator_code)
            .unwrap_or(false)
        {
            println!("short operator code, we don't need to check if valid");
        } else if !is_valid_number(&phone_number) {
            if !self.channel.is_answered().await {
                let context = self.channel.get_call_context().await?;
                let answer_sdp = if let Ok(sdp) = self.channel.get_local_sdp().await
                {
                    sdp
                } else {
                    self.channel.create_answer_sdp(false).await?
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
                            self.channel.id.clone(),
                            183,
                            "Session Progress".to_string(),
                            answer_sdp.to_string(),
                        )
                        .await?;
                }
            }
            Channel::play_sound_to_channel(
                &self.channel.id,
                None,
                vec!["tts://You've dialled an invalid number".to_string()],
                1,
                true,
            )
            .await?;
            return Err(anyhow!("invalid number"));
        }
        if let Some(internal_number) = SWITCHBOARD_SERVICE
            .db
            .get_number("admin", &e164[1..].to_string())
            .await
        {
            if internal_number.tenant_id.clone().unwrap_or("".to_string())
                == tenant_id
            {
                self.channel
                    .update_cdr(CdrItemChange {
                        call_type: Some("internal".to_string()),
                        ..Default::default()
                    })
                    .await?;
                self.switch_internal_number(&internal_number).await?;
                return Ok(());
            }
        }

        self.channel
            .update_cdr(CdrItemChange {
                call_type: Some("outbound".to_string()),
                to_type: Some("number".to_string()),
                to_exten: if operator_code.is_some() {
                    Some(number.to_string())
                } else {
                    Some(e164[1..].to_string())
                },
                operator_code: operator_code.as_ref().map(|(c, _)| c.to_string()),
                ..Default::default()
            })
            .await?;
        let flow_id = self.channel.id.clone();
        let flow = serde_json::from_value(serde_json::json!({
            "start": {
                "module": "start",
                "next": "extension"
            },
            "extension": {
                "module": "extension",
                "extensions": [
                    {
                        "type": "forward",
                        "number": if operator_code.is_some() { number.to_string() } else { e164 },
                        "duration": 300,
                        "outbound_call": true,
                    }
                ]
            }
        }))?;
        self.callflow(tenant_id, flow_id, flow, false).await?;
        Ok(())
    }

    pub async fn callerid_match_patterns(
        &self,
        patterns: Vec<String>,
    ) -> Result<bool> {
        let context = self.channel.get_call_context().await?;
        match &context.endpoint {
            Endpoint::Provider {
                number, anonymous, ..
            } => {
                let callerid = if number.len() > 1 {
                    number[1..].to_string()
                } else {
                    number.to_string()
                };
                for pattern in patterns {
                    if pattern == "anonymous" {
                        if *anonymous {
                            return Ok(true);
                        }
                    } else {
                        if let Ok(re) = Regex::new(&pattern) {
                            if re.is_match(&callerid) {
                                return Ok(true);
                            }
                        }
                    }
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn get_sipuser_flow(&self, user: &User) -> Result<serde_json::Value> {
        let context = self.channel.get_call_context().await?;

        let mut flow = serde_json::json!({
            "start": {
                "module": "start",
                "next": "extension"
            },
            "extension": {
                "module": "extension",
                "duration": user.duration,
                "noanswer": "noanswer",
                "busy": "busy",
                "unavailable": "unavailable",
                "extensions": [
                    {
                        "type": "sipuser",
                        "uuid": &user.uuid.to_string(),
                    }
                ]
            }
        });

        if context.redirect_peer.is_some() {
            return Ok(flow);
        }

        println!("user call handling {:?}", user.call_handling);
        if let Some(call_handling) = user.call_handling() {
            if !context.forward {
                if context.endpoint.is_internal_call() {
                    let number = call_handling
                        .forward_internal_number
                        .unwrap_or("".to_string());
                    if number != ""
                        && call_handling.forward_internal.unwrap_or(false)
                    {
                        return Ok(serde_json::json!({
                            "start": {
                                "module": "start",
                                "next": "forward",
                            },
                            "forward": {
                                "module": "ForwardCall",
                                "number": number,
                            },
                        }));
                    }
                } else {
                    let number = call_handling
                        .forward_external_number
                        .unwrap_or("".to_string());
                    if number != ""
                        && call_handling.forward_external.unwrap_or(false)
                    {
                        return Ok(serde_json::json!({
                            "start": {
                                "module": "start",
                                "next": "forward",
                            },
                            "forward": {
                                "module": "ForwardCall",
                                "number": number,
                            },
                        }));
                    }
                }
            }

            // if the call is a blind transfer, check if we return to sender
            if context.refer.is_some() {
                if call_handling
                    .blind_transfer_return_to_sender
                    .unwrap_or(false)
                {
                    let sender = context
                        .endpoint
                        .user()
                        .ok_or(anyhow!("non-user can't transfer a call"))?;
                    flow["noanswer"] = json!({
                        "module": "extension",
                        "duration": sender.duration,
                        "caller_id": {
                            "user": user.extension.unwrap_or(0).to_string(),
                            "display_name": format!("Returned transfer from {}",
                                            user.display_name.clone().unwrap_or("".to_string())),
                            "anonymous": false,
                        },
                        "extensions": [
                            {
                                "type": "sipuser",
                                "uuid": &sender.uuid
                            }
                        ]
                    });
                    return Ok(flow);
                }
            }

            if let Some(noanswer_flow_id) = call_handling.noanswer_callflow_id {
                flow["noanswer"] = json!({
                    "module": "callflow",
                    "callflow_uuid": noanswer_flow_id,
                });
            } else if let Some(noanswer_flow) = call_handling.noanswer_callflow {
                flow["noanswer"] = json!({
                    "module": "CallflowContent",
                    "callflow_content": noanswer_flow,
                });
            }

            if let Some(flow_id) = call_handling.busy_callflow_id {
                flow["busy"] = json!({
                    "module": "callflow",
                    "callflow_uuid": flow_id,
                });
            } else if let Some(flow_content) = call_handling.busy_callflow {
                flow["busy"] = json!({
                    "module": "CallflowContent",
                    "callflow_content": flow_content,
                });
            }

            if let Some(flow_id) = call_handling.unavailable_callflow_id {
                flow["unavailable"] = json!({
                    "module": "callflow",
                    "callflow_uuid": flow_id,
                });
            } else if let Some(flow_content) = call_handling.unavailable_callflow {
                flow["unavailable"] = json!({
                    "module": "CallflowContent",
                    "callflow_content": flow_content,
                });
            }
        } else {
            if context.backup.is_none() {
                if user.transfer_mailbox {
                    if let Some(voicemail_user) = SWITCHBOARD_SERVICE
                        .db
                        .get_user_voicemail(&user.uuid.to_string())
                        .await
                    {
                        flow["noanswer"] = serde_json::json!({
                            "module": "voicemail",
                            "mailboxes": [voicemail_user.uuid],
                        });
                    }
                } else if let Some(backup) = user.transfer_backup_extension {
                    let backup = if backup == -1 {
                        if context.refer.is_some() {
                            context
                                .endpoint
                                .user()
                                .ok_or(anyhow!("non-user can't have backup"))?
                                .extension
                                .unwrap_or(0)
                        } else {
                            0
                        }
                    } else {
                        backup
                    };
                    if backup != 0 {
                        flow["noanswer"] = serde_json::json!({
                            "user": user,
                            "module": "backupextension",
                            "extension": backup,
                        });
                    }
                }
            }
        }

        Ok(flow)
    }

    pub async fn switch_user(
        &self,
        tenant_id: &str,
        user_id: &str,
        send_to_vm: bool,
    ) -> Result<()> {
        if send_to_vm {
            let vm_user = SWITCHBOARD_SERVICE
                .db
                .get_user_voicemail(user_id)
                .await
                .ok_or_else(|| anyhow!("No Mailbox on user"))?;

            let context = self.channel.get_call_context().await?;
            // let force_leave_message = cli.strip_suffix("**").is_some();
            return self.switch_voicemail_menu(&context, &vm_user, true).await;
        }

        let user = SWITCHBOARD_SERVICE
            .db
            .get_user(tenant_id, user_id)
            .await
            .ok_or_else(|| anyhow!("no such user"))?;

        let local_channel = self.channel.clone();
        let local_user = user.clone();
        tokio::spawn(async move {
            if let Some(cdr) = local_channel.get_cdr().await {
                if cdr.to_uuid.as_ref() == Some(&local_user.uuid) {
                    let _ = local_channel
                        .update_cdr(CdrItemChange {
                            to_user: Some(local_user.name.clone()),
                            to_name: Some(
                                local_user
                                    .display_name
                                    .clone()
                                    .unwrap_or_else(|| "".to_string()),
                            ),
                            ..Default::default()
                        })
                        .await;
                }
            }
        });

        let unlimited_recording = user.unlimited_recording.unwrap_or(false);

        let mut recording_enabled = false;
        if user.recording.unwrap_or(false) {
            if let Ok(context) = self.channel.get_call_context().await {
                if context.endpoint.user().is_none() {
                    recording_enabled = true;
                }
            }
        }
        self.channel
            .update_recording(tenant_id, recording_enabled, unlimited_recording)
            .await?;

        let flow = serde_json::from_value(self.get_sipuser_flow(&user).await?)?;
        self.callflow(
            &user.tenant_id,
            self.channel.id.clone(),
            flow,
            unlimited_recording,
        )
        .await
    }

    /**
        For offnet connect; here we want to parse the details,
        check if the number is in the allowed cli list, check
        the pin and get a number to call out to.

        Responses:
         1) Call is not initial leg / from number
           - Ok(None) -> Continue call flow
         2) Number calling in is not a vlid number in allowe list
           - Ok(None) -> Continue call flow
         2) Multiple pin failure
           - Err -> End Call
         3) Invalid callout number
           - Err -> End Call
         4) Call from number valid & call to number valid
           - Ok(callout_number) -> Make offnet connect
    */
    pub async fn get_offnet_connect_endpoints(
        &self,
        offnet_connect: OffnetConnect,
    ) -> anyhow::Result<Option<String>> {
        info!("Now try get offnet connect details");

        let mut context = self.channel.get_call_context().await?;

        // None checked above
        let Some(internal_number) = context.to_internal_number.as_ref() else {
            return Ok(None);
        };

        // Only allowing for direct calls
        if context.refer.is_some() {
            return Ok(None);
        }

        // Need to check that calling in number is in an allowed list
        let number = if let Endpoint::Provider { number, .. } = &context.endpoint {
            get_phone_number(number)
                .ok_or(anyhow!("Failed to parse origin number"))?
        } else {
            return Ok(None);
        };

        let is_allowed_cli =
            offnet_connect.allowed_cli_list.iter().any(|allowed_cli| {
                get_phone_number(allowed_cli)
                    .map(|num| num.eq(&number))
                    .unwrap_or_default()
            });

        if !is_allowed_cli {
            // If the number is not registered as a desired start point
            // We return to let the flow continue
            return Ok(None);
        }

        // Now check pin input
        let auth = self
            .channel
            .pin_auth(
                &offnet_connect.pin,
                vec![format!("tts://{}", offnet_connect.prompt.unwrap_or("Authorised Caller ID detected, please input pin to dial out from this number".to_string()))],
                vec!["tts://Incorrect pin".to_string()],
                10,
                offnet_connect.pin_timeout
            )
            .await?;

        match auth {
            AuthPinResult::Accepted => {}
            AuthPinResult::Failed => {
                let _ = self.channel.play_sound(None, vec!["tts://Failed to authenticate for offnet connect, contact administrator or try again".to_string()], true).await;
                return Err(anyhow!("Failed to authenticate"));
            }
            AuthPinResult::Timeout => {
                let _ = self
                    .channel
                    .play_sound(
                        None,
                        vec!["tts://Authentication timeout, continue routing"
                            .to_string()],
                        true,
                    )
                    .await;
                return Ok(None);
            }
        }

        // Now take in number to call
        let digits = self
            .channel
            .play_sound_and_get_dtmf(
                vec!["tts://Enter number you want to call, then press the # key"
                    .to_string()],
                100_000,
                5000,
                100,
            )
            .await
            .join("");

        let number =
            get_phone_number(&digits).ok_or(anyhow!("Failed to parse number"))?;

        if !is_valid_number(&number) {
            return Err(anyhow!("Invalid phone number"));
        }

        let e164 = number.format().mode(phonenumber::Mode::E164).to_string();

        let cli = offnet_connect
            .caller_id
            .unwrap_or(internal_number.e164.clone());

        context.set_caller_id = Some(cli);
        self.channel.set_call_context(&context).await?;

        Ok(Some(e164))
    }

    pub async fn switch_extension(
        &self,
        extension: Extension,
        force_leave_message: bool,
    ) -> Result<(), Error> {
        println!("switch extension");
        let (flow_id, flow) = match extension.discriminator.as_ref() {
            "sipuser" => {
                self.switch_user(
                    &extension.tenant_id,
                    &extension.parent_id,
                    force_leave_message,
                )
                .await?;
                return Ok(());
            }
            "huntgroup" => {
                let group = SWITCHBOARD_SERVICE
                    .db
                    .get_group(&extension.tenant_id, &extension.parent_id)
                    .await
                    .ok_or(anyhow!("no such hunt group"))?;

                {
                    let channel = self.channel.clone();
                    tokio::spawn(async move {
                        if let Some(cdr) = channel.get_cdr().await {
                            if cdr.to_uuid == Some(group.uuid) {
                                let _ = channel
                                    .update_cdr(CdrItemChange {
                                        to_name: Some(group.name.clone()),
                                        ..Default::default()
                                    })
                                    .await;
                            }
                        }
                    });
                }

                (
                    self.channel.id.clone(),
                    serde_json::from_value(serde_json::json!({
                        "start": {
                            "module": "start",
                            "next": "extension"
                        },
                        "extension": {
                            "module": "extension",
                            "extensions": [
                                {
                                    "type": "huntgroup",
                                    "uuid": &extension.parent_id
                                }
                            ]
                        }
                    }))?,
                )
            }
            "callflow" => {
                let callflow = SWITCHBOARD_SERVICE
                    .db
                    .get_callflow(&extension.tenant_id, &extension.parent_id)
                    .await
                    .ok_or(SwitchboardError::InvalidCallFlow)?;
                {
                    let channel = self.channel.clone();
                    let callflow = callflow.clone();
                    tokio::spawn(async move {
                        if let Some(cdr) = channel.get_cdr().await {
                            if cdr.to_uuid == Some(callflow.uuid.clone()) {
                                let _ = channel
                                    .update_cdr(CdrItemChange {
                                        callflow_uuid: Some(callflow.uuid.clone()),
                                        callflow_name: Some(callflow.name.clone()),
                                        ..Default::default()
                                    })
                                    .await;
                            }
                        }
                    });
                }
                self.switch_callflow(&callflow).await?;
                return Ok(());
            }
            "voicemailmenu" => {
                let menu = SWITCHBOARD_SERVICE
                    .db
                    .get_voicemail_menu(&extension.tenant_id, &extension.parent_id)
                    .await
                    .ok_or(anyhow!("no voicemail menu"))?;
                let mailbox = if menu.mailbox == "personal" {
                    let context = self.channel.get_call_context().await?;
                    let user =
                        context.endpoint.user().ok_or(anyhow!("not from a user"))?;
                    user.personal_vmbox.clone().unwrap_or("".to_string())
                } else {
                    menu.mailbox.to_string()
                };
                let vm_user = SWITCHBOARD_SERVICE
                    .db
                    .get_voicemail_user(&extension.tenant_id, &mailbox)
                    .await
                    .ok_or(anyhow!("no mailbox found"))?;
                let context = self.channel.get_call_context().await?;
                self.switch_voicemail_menu(&context, &vm_user, force_leave_message)
                    .await?;
                return Ok(());
            }
            _ => return Ok(()),
        };

        self.callflow(&extension.tenant_id, flow_id, flow, false)
            .await
    }

    pub async fn callflow(
        &self,
        tenant_id: &str,
        flow_id: String,
        flow: HashMap<String, Value>,
        unlimited_recording: bool,
    ) -> Result<(), Error> {
        REDIS
            .hset(
                &format!("nebula:channel:{}", &self.channel.id),
                "tenant_uuid",
                tenant_id,
            )
            .await?;
        REDIS
            .hset(
                &format!("nebula:channel:{}", &self.channel.id),
                "callflow",
                &serde_json::to_string(&flow)?,
            )
            .await?;
        REDIS
            .hset(
                &format!("nebula:channel:{}", &self.channel.id),
                "callflow_id",
                &serde_json::to_string(&flow_id)?,
            )
            .await?;
        let callflow = CallFlow::new(
            flow_id,
            self.clone(),
            flow,
            tenant_id.to_string(),
            unlimited_recording,
        );
        callflow.start().await
    }

    pub async fn voicemail(
        &self,
        voicemail_user: &VoicemailUser,
        access_code: &str,
    ) -> Result<()> {
        if self.channel.is_hangup().await {
            return Ok(());
        }
        let greeting = if !voicemail_user.greeting.is_empty() {
            &voicemail_user.greeting
        } else {
            "283980ad-97a6-4f06-9b51-4b10b3f0d23a"
        };
        let mut sounds = vec![greeting.to_string()];
        if voicemail_user.beep {
            sounds.push("tone://200,0,500,0".to_string());
        }
        self.channel.answer().await?;

        if !access_code.is_empty() {
            let pressed = self
                .channel
                .play_sound_and_get_dtmf(sounds, 0, 5000, access_code.len())
                .await
                .join("");
            if pressed == access_code {
                self.voicemail_menu(voicemail_user, true).await?;
                return Ok(());
            }
        } else {
            self.channel.play_sound(None, sounds, true).await?;
        }
        let context = self.channel.get_call_context().await?;
        let mut callerid = match &context.endpoint {
            Endpoint::User { user, .. } => user.extension.unwrap_or(0).to_string(),
            Endpoint::Provider {
                number, anonymous, ..
            } => {
                if *anonymous {
                    "anonymous".to_string()
                } else {
                    number.to_string()
                }
            }
            Endpoint::Trunk { number, .. } => number.to_string(),
        };

        let mut refer_ctx = context
            .refer
            .filter(|r| r.direction == CallDirection::Recipient)
            .clone();

        if refer_ctx.is_some() {
            while let Some(refer) = refer_ctx {
                let peer_channel = Channel::new(refer.peer);
                let inbound_channel = peer_channel
                    .get_inbound_channel()
                    .await
                    .unwrap_or(peer_channel);

                let Ok(peer_ctx) = inbound_channel.get_call_context().await else {
                    break;
                };

                refer_ctx =
                    if peer_ctx.refer.as_ref().is_some_and(|refer| {
                        refer.direction == CallDirection::Recipient
                    }) {
                        peer_ctx.refer
                    } else {
                        callerid = match peer_ctx.endpoint {
                            Endpoint::User { .. } => peer_ctx.to,
                            Endpoint::Provider {
                                number, anonymous, ..
                            } => {
                                if anonymous {
                                    "anonymous".to_string()
                                } else {
                                    number.to_string()
                                }
                            }
                            Endpoint::Trunk { number, .. } => number.to_string(),
                        };

                        None
                    };
            }
        }

        let calleeid = context.to.to_string();
        let voicemail_id = context
            .cdr
            .map(|cdr| cdr.to_string())
            .unwrap_or_else(|| uuid_v5(&self.channel.id).to_string());
        self.channel.start_voicemail(voicemail_id.clone()).await?;
        let time = SystemTime::now();
        self.channel
            .update_cdr(CdrItemChange {
                answer_type: Some("mailbox".to_string()),
                answer_uuid: Some(voicemail_user.uuid.clone()),
                answer_name: Some(voicemail_user.name.clone()),
                answer_exten: Some(voicemail_user.mailbox.clone()),
                ..Default::default()
            })
            .await?;
        self.channel
            .receive_event("0", ChannelEvent::Hangup)
            .await?;
        let finish = SystemTime::now();
        let duration = finish.duration_since(time)?.as_millis() as i64;

        if duration < 1000 {
            // if duration is less than 1 sec, don't create voicemail
            return Ok(());
        }

        if let Some(duration_limit) = voicemail_user.duration_limit {
            if duration < duration_limit * 1000 {
                // if duration is less than duration limit, don't create voicemail
                return Ok(());
            }
        }

        SWITCHBOARD_SERVICE
            .db
            .create_voicemail(
                voicemail_id.clone(),
                voicemail_user.uuid.clone(),
                callerid,
                calleeid,
                duration / 1000,
                time,
            )
            .await?;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(20)) => {}
            _ = self.channel.receive_event("0", ChannelEvent::VoicemailUploaded) => {}
        }

        let vm_user_uuid = voicemail_user.uuid.clone();
        tokio::spawn(async move {
            let _ = SWITCHBOARD_SERVICE
                .yaypi_client
                .voicemail_msg_notify(&vm_user_uuid, &voicemail_id)
                .await;
        });
        let _ = Channel::notify_voicemail(&voicemail_user).await;
        Ok(())
    }

    pub async fn switch_voicemail_menu(
        &self,
        context: &CallContext,
        voicemail_user: &VoicemailUser,
        force_leave_message: bool,
    ) -> Result<()> {
        if force_leave_message || context.refer.is_some() {
            self.voicemail(voicemail_user, "").await?;
        } else {
            self.voicemail_menu(voicemail_user, false).await?;
        }

        Ok(())
    }

    pub async fn voicemail_menu(
        &self,
        mailbox: &VoicemailUser,
        require_password: bool,
    ) -> Result<()> {
        let mut mailbox = mailbox.clone();
        if require_password && mailbox.password.is_empty() {
            self.channel
                .play_sound(
                    None,
                    vec!["fba91cb1-c938-4e50-b75e-c3c0990600f0".to_string()],
                    true,
                )
                .await?;
            return Ok(());
        }

        if mailbox.password != "" {
            let auth = self
                .channel
                .pin_auth(
                    &mailbox.password,
                    vec!["tts://please enter your password".to_string()],
                    vec!["tts://login incorrect".to_string()],
                    10,
                    None,
                )
                .await?;
            if auth != AuthPinResult::Accepted {
                return Ok(());
            }
        }

        let msgs = SWITCHBOARD_SERVICE
            .db
            .get_voicemail_messages(&mailbox.uuid)
            .await
            .unwrap_or(vec![]);
        let mut new_msgs: Vec<VoicemailMessage> = msgs
            .iter()
            .filter_map(|m| {
                if m.folder == "new" {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut saved_msgs: Vec<VoicemailMessage> = msgs
            .iter()
            .filter_map(|m| {
                if m.folder == "saved" {
                    Some(m.clone())
                } else {
                    None
                }
            })
            .collect();

        let msg_text = format!(
            "you have {} new and {} saved messages.",
            new_msgs.len(),
            saved_msgs.len(),
        );

        let menu_text = format!(
            r"
          Press {} for new messages.
          Press {} for saved messages.
          Press {} to change the password.
          Press {} to record the greeting.
          Press {} to play the greeting.
          ",
            VMNEW, VMSAVED, VMPASSWORD, VMRECORDGREETING, VMPLAYGREETING,
        );

        let mut first_play = true;
        loop {
            let text = if first_play {
                first_play = false;
                msg_text.clone() + &menu_text
            } else {
                menu_text.clone()
            };

            let dtmf = self
                .channel
                .play_sound_and_get_dtmf(
                    vec![format!("tts://{}", &text)],
                    5000,
                    5000,
                    1,
                )
                .await
                .join("");
            match dtmf.as_str() {
                VMNEW => {
                    new_msgs = self
                        .play_voicemail_messages(&mailbox, &new_msgs, "new")
                        .await;
                }
                VMSAVED => {
                    saved_msgs = self
                        .play_voicemail_messages(&mailbox, &saved_msgs, "saved")
                        .await;
                }
                VMPASSWORD => {
                    self.change_voicemail_password(&mailbox).await;
                }
                VMRECORDGREETING => {
                    let _ = self.record_voicemail_greeting(&mut mailbox).await;
                }
                VMPLAYGREETING => {
                    if !mailbox.greeting.is_empty() {
                        let _ = self
                            .channel
                            .play_sound(
                                None,
                                vec![mailbox.greeting.to_string()],
                                true,
                            )
                            .await;
                    } else {
                        let _ = self
                            .channel
                            .play_sound(
                                None,
                                vec!["tts://greeting not available".to_string()],
                                true,
                            )
                            .await;
                    }
                }
                _ => {}
            }
            if self.channel.is_hangup().await {
                return Ok(());
            }
        }
    }

    async fn record_voicemail_greeting(
        &self,
        vm_user: &mut VoicemailUser,
    ) -> Result<()> {
        let mut first_play = true;
        let mut play_greeting = false;

        let mut duration = 0.0;
        let sound_uuid = nebula_utils::uuid();
        loop {
            if first_play {
                first_play = false;
                let record_text = "tts://record your message at the tone, press any key to end the recording".to_string();
                self.channel
                    .play_sound(
                        None,
                        vec![record_text, "tone://200,0,500,0".to_string()],
                        true,
                    )
                    .await?;
                let start = SystemTime::now();
                self.channel.record_to_file(sound_uuid.clone()).await?;
                tokio::select! {
                    _ = self.channel.receive_event("$", ChannelEvent::Dtmf) => {

                    }
                    _ = self.channel.receive_event("0", ChannelEvent::Hangup) => {
                        break;
                    }
                }
                duration = start.elapsed()?.as_millis() as f64 / 1000.0;
                self.channel.stop_record_to_file(sound_uuid.clone()).await?;
            }

            let mut sounds = Vec::new();
            if play_greeting {
                play_greeting = false;
                sounds.push("record_file://".to_string() + sound_uuid.as_str());
            }
            let menu = format!(
                r#"tts://
                Press {} to save the recorded greeting.
                Press {} to listen to the recorded greeting.
                Press {} to rerecord the greeting.
                Press {} to exit.
                "#,
                VMSAVERECORDGREETING,
                VMLISTENRECORDGREETING,
                VMRERECORDGREETING,
                VMEXITRECORDGREETING,
            );
            sounds.push(menu);
            let digits = self
                .channel
                .play_sound_and_get_dtmf(sounds, 5000, 5000, 1)
                .await;
            let pressed = digits.join("");
            match pressed.as_str() {
                VMSAVERECORDGREETING => {
                    self.channel
                        .upload_record_to_file(sound_uuid.clone())
                        .await?;

                    let tenant_id = vm_user.tenant_id.clone();
                    let local_sound_uuid = sound_uuid.clone();
                    let vm_user_id = vm_user.id;
                    tokio::spawn(async move {
                        let _ = SWITCHBOARD_SERVICE
                            .db
                            .create_sound(
                                local_sound_uuid.clone(),
                                tenant_id.clone(),
                                "Voicemail Greeting".to_string(),
                                duration,
                            )
                            .await;
                        let _ = SWITCHBOARD_SERVICE
                            .yaypi_client
                            .create_sound(
                                &local_sound_uuid,
                                &tenant_id,
                                "Voicemail Greeting",
                            )
                            .await;
                        SWITCHBOARD_SERVICE
                            .db
                            .update_voicemail_greeting(vm_user_id, &local_sound_uuid)
                            .await;
                    });
                    self.channel
                        .play_sound(None, vec!["greeting saved".to_string()], true)
                        .await?;
                    vm_user.greeting = sound_uuid;
                    return Ok(());
                }
                VMLISTENRECORDGREETING => {
                    play_greeting = true;
                }
                VMRERECORDGREETING => {
                    first_play = true;
                }
                VMEXITRECORDGREETING => {
                    return Ok(());
                }
                _ => (),
            };
            if self.channel.is_hangup().await {
                return Ok(());
            }
        }

        Ok(())
    }

    async fn change_voicemail_password(&self, vm_user: &VoicemailUser) {
        let sounds = vec![
            "tts://Please enter your new password, followed by the hash key."
                .to_string(),
        ];
        let pressed = self
            .channel
            .play_sound_and_get_dtmf(sounds.clone(), 5000, 5000, 128)
            .await
            .join("");
        let voicemail_uuid = vm_user.uuid.clone();
        SWITCHBOARD_SERVICE
            .db
            .update_voicemail_password(vm_user, &pressed)
            .await;
        let pass_tts = if !pressed.is_empty() {
            "tts://Your password has been changed."
        } else {
            "tts://Your password has been cleared."
        };

        tokio::spawn(async move {
            let _ = SWITCHBOARD_SERVICE
                .yaypi_client
                .notify_voicemail_pin(voicemail_uuid, pressed)
                .await;
        });

        let _ = self
            .channel
            .play_sound(None, vec![pass_tts.to_string()], true)
            .await;
    }

    async fn play_voicemail_messages(
        &self,
        vm_user: &VoicemailUser,
        vm_msgs: &[VoicemailMessage],
        folder: &str,
    ) -> Vec<VoicemailMessage> {
        let mut vm_msgs = vm_msgs.to_vec();
        let context = self.channel.get_call_context().await.ok();
        let menu_text = format!(
            r"
            Press {} to play the next message.
            Press {} to play the previous message.
            Press {} to repeat this message.
            Press {} to delete this message.
            Press {} for the main menu.
            ",
            VMNEXT, VMPREVIOUS, VMREPEAT, VMDELETE, VMMAINMENU,
        );

        let mut i = 0;
        loop {
            if vm_msgs.is_empty() {
                let _ = self
                    .channel
                    .play_sound(
                        None,
                        vec!["tts://no more messages".to_string()],
                        true,
                    )
                    .await;
                return vm_msgs;
            }
            let msg = &vm_msgs[i];
            if folder == "new" {
                SWITCHBOARD_SERVICE.db.save_voicemail_msg(msg).await;
                {
                    let vm_uuid = msg.uuid.clone();
                    tokio::spawn(async move {
                        let _ = SWITCHBOARD_SERVICE
                            .yaypi_client
                            .voicemail_msg_notify_saved(&vm_uuid)
                            .await;
                    });
                }
                let _ = Channel::notify_voicemail(vm_user).await;
            }

            let mut first_play = true;
            'play: loop {
                let sounds = if first_play {
                    first_play = false;

                    let mut sounds = vec![
                        format!("tts://<say-as interpret-as=\"ordinal\">{}</say-as> message.", i+1),
                    ];

                    if vm_user.play_timestamp {
                        let timezone = context
                            .as_ref()
                            .and_then(|c| c.endpoint.user())
                            .and_then(|u| u.timezone.as_ref())
                            .map(|t| t.as_str());
                        let time = DateTime::<Utc>::from(msg.time)
                            .with_timezone(&timezone_tz(timezone));
                        let now = now(timezone);

                        let date_tts = if time
                            .add(
                                chrono::Duration::from_std(Duration::from_secs(
                                    60 * 60 * 24,
                                ))
                                .unwrap(),
                            )
                            .format("%d-%B-%Y")
                            .to_string()
                            == now.format("%d-%B-%Y").to_string()
                        {
                            "yesterday".to_string()
                        } else if time.format("%d-%B-%Y").to_string()
                            == now.format("%d-%B-%Y").to_string()
                        {
                            "today".to_string()
                        } else {
                            ",on ".to_string()
                                + &time.format("%A, %d-%B-%Y").to_string()
                        };
                        sounds.push(format!(
                            "tts://received at {} {}",
                            time.format("%l:%M %P"),
                            date_tts
                        ));
                    }

                    if vm_user.play_callerid && !msg.callerid.is_empty() {
                        let callerid_tts = if msg.callerid == "anonymous"
                            || msg.callerid == "unavailable"
                        {
                            "tts://from Anonymous or Withheld number".to_string()
                        } else {
                            format!("tts://from <say-as interpret-as=\"telephone\">{}</say-as>", msg.callerid)
                        };
                        sounds.push(callerid_tts);
                    }
                    sounds.push("vm://".to_string() + &msg.uuid);
                    sounds.push("tts://".to_string() + &menu_text);

                    sounds
                } else {
                    vec!["tts://".to_string() + &menu_text]
                };

                let dtmf = self
                    .channel
                    .play_sound_and_get_dtmf(sounds, 5000, 5000, 1)
                    .await
                    .join("");
                match dtmf.as_str() {
                    VMNEXT => {
                        i += 1;
                        if i >= vm_msgs.len() {
                            i = 0;
                        }
                        break 'play;
                    }
                    VMPREVIOUS => {
                        if i == 0 {
                            i = vm_msgs.len() - 1;
                        } else {
                            i -= 1;
                        }
                        break 'play;
                    }
                    VMREPEAT => {
                        break 'play;
                    }
                    VMDELETE => {
                        let client = storagev1::StorageClient::new();
                        let _ = client
                            .delete_object(
                                "aura-voicemails",
                                &(msg.uuid.clone() + ".mp3"),
                            )
                            .await;
                        let _ = client
                            .delete_object(
                                "aura-voicemails",
                                &(msg.uuid.clone() + ".wav"),
                            )
                            .await;
                        SWITCHBOARD_SERVICE.db.delete_voicemail_msg(msg).await;

                        let vm_user_uuid = vm_user.uuid.clone();
                        let vm_msg_uuid = msg.uuid.clone();
                        {
                            let vm_user = vm_user.clone();
                            tokio::spawn(async move {
                                let _ = SWITCHBOARD_SERVICE
                                    .yaypi_client
                                    .voicemail_msg_delete(
                                        &vm_user_uuid,
                                        &vm_msg_uuid,
                                    )
                                    .await;
                                let _ = Channel::notify_voicemail(&vm_user).await;
                            });
                        }
                        vm_msgs.remove(i);
                        if i >= vm_msgs.len() && !vm_msgs.is_empty() {
                            i = vm_msgs.len() - 1;
                        }

                        break 'play;
                    }
                    VMMAINMENU => {
                        return vm_msgs;
                    }
                    _ => {
                        if self.channel.is_hangup().await {
                            return vm_msgs;
                        }
                    }
                }
            }
        }
    }
}

pub fn get_phone_number(exten: &str) -> Option<phonenumber::PhoneNumber> {
    let exten = if exten.len() >= 16 {
        // special prefix for surevoip
        exten
            .strip_prefix("521800")
            .or_else(|| exten.strip_prefix("521801"))
            .or_else(|| exten.strip_prefix("504043"))
            .or_else(|| exten.strip_prefix("505043"))
            .unwrap_or(exten)
    } else {
        exten
    };

    let exten = if !exten.starts_with('+') && !exten.starts_with('0') {
        format!("+{}", exten)
    } else {
        exten.to_string()
    };

    phonenumber::parse(Some(phonenumber::country::GB), exten).ok()
}

pub async fn get_internal_number(exten: &str) -> Option<Number> {
    let number = get_phone_number(exten)?;
    let e164 = number.format().mode(phonenumber::Mode::E164).to_string();
    SWITCHBOARD_SERVICE
        .db
        .get_number("admin", &e164[1..].to_string())
        .await
}

pub fn timezone_tz(timezone: Option<&str>) -> Tz {
    let tz = match timezone {
        Some(tz) => match tz.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_e) => UTC,
        },
        None => UTC,
    };
    tz
}

pub fn now(timezone: Option<&str>) -> DateTime<Tz> {
    let tz = match timezone {
        Some(tz) => match tz.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_e) => UTC,
        },
        None => UTC,
    };
    Utc::now().with_timezone(&tz)
}

pub fn check_time(time: &DateTime<Tz>, time_str: &str) -> Option<bool> {
    let parts: Vec<&str> = time_str.split(",").collect();
    if parts.len() < 4 {
        return None;
    }
    let year = if parts.len() == 5 {
        check_formatted_time(time, parts[4], "year")?
    } else {
        true
    };
    Some(
        check_formatted_time(time, parts[0], "time_range")?
            && check_formatted_time(time, parts[1], "weekday")?
            && check_formatted_time(time, parts[2], "day")?
            && check_formatted_time(time, parts[3], "month")?
            && year,
    )
}

fn check_formatted_time(
    time: &DateTime<Tz>,
    time_str: &str,
    format: &str,
) -> Option<bool> {
    if time_str == "*" {
        return Some(true);
    }
    match format {
        "day" => Some(time.date().day() == time_str.parse::<u32>().ok()?),
        "month" => Some(
            time.date().month()
                == Month::from_str(time_str).ok()?.number_from_month(),
        ),
        "weekday" => {
            Some(time.date().weekday() == Weekday::from_str(time_str).ok()?)
        }
        "year" => Some(time.date().year() == time_str.parse::<i32>().ok()?),
        "time_range" => {
            let time = time.time();
            let current_time_str =
                format!("{:0>2}:{:0>2}", time.hour(), time.minute());
            let time = NaiveTime::parse_from_str(&current_time_str, "%H:%M").ok()?;
            let parts: Vec<&str> = time_str.split("-").collect();
            if parts.len() == 1 {
                Some(time_str == &current_time_str)
            } else if parts.len() == 2 {
                let start = NaiveTime::parse_from_str(parts[0], "%H:%M").ok()?;
                let end = NaiveTime::parse_from_str(parts[1], "%H:%M").ok()?;
                if start <= end {
                    Some(start <= time && time <= end)
                } else {
                    Some(!(end < time && time < start))
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn shortcode_user(
    tenant_id: &str,
    shortcode: &str,
    exten: &str,
) -> Option<User> {
    let exten = exten[shortcode.len() + 1..]
        .to_string()
        .parse::<i64>()
        .ok()?;
    let extension = SWITCHBOARD_SERVICE
        .db
        .get_extension(tenant_id, exten)
        .await?;
    if extension.discriminator != "sipuser" {
        return None;
    }
    SWITCHBOARD_SERVICE
        .db
        .get_user(tenant_id, &extension.parent_id)
        .await
}

async fn update_user_call_forward(user: &User, exten: &str) {
    let forward = if exten.is_empty() {
        None
    } else {
        Some(exten.to_string())
    };
    SWITCHBOARD_SERVICE
        .db
        .update_user_global_forward(user.uuid.clone(), forward)
        .await;
}

pub fn format_national_number(number: &PhoneNumber) -> String {
    number
        .format()
        .mode(phonenumber::Mode::National)
        .to_string()
        .replace([' ', '(', ')'], "")
}
