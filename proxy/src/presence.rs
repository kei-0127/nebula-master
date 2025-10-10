use std::{collections::HashMap, str::FromStr};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use nebula_db::{
    message::{CallDirection, DialogInfo, DialogState},
    models::User,
};
use nebula_redis::REDIS;
use nebula_utils::sha256;
use serde::{Deserialize, Serialize};
use sip::{
    message::{Address, Cseq, Message, Method, Uri},
    transaction::TM,
};
use strum_macros::EnumString;

use crate::server::{get_tenant_switchboard_host, PRPOXY_SERVICE};

const CSEQ: &str = "cseq";
const EVENT: &'static str = "event";

pub struct Presence {}

#[derive(
    strum_macros::Display,
    EnumString,
    PartialEq,
    Clone,
    Serialize,
    Deserialize,
    Debug,
)]
pub enum EventKind {
    #[strum(serialize = "presence")]
    Presence,
    #[strum(serialize = "dialog")]
    Dialog,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum PrensentityKind {
    User,
    Mailbox,
    ParkingSlot,
    QueueAvailability,
    QueueLoginLogout,
}

impl EventKind {
    fn content_type(&self) -> String {
        match &self {
            EventKind::Presence => "application/pidf+xml".to_string(),
            EventKind::Dialog => "application/dialog-info+xml".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Watcher {
    pub watcher_id: String,
    pub presentity_id: String,
    pub presentity_kind: PrensentityKind,
    pub presentity_user: String,
    pub presentity_domain: String,
    pub event_kind: EventKind,
    pub from: Address,
    pub to: Address,
    pub contact: Address,
    pub callid: String,
    pub remote_uri: Uri,
}

impl Watcher {
    async fn from_msg(msg: &Message) -> Result<Watcher> {
        let watcher_id = sha256(&format!(
            "{}{}",
            &msg.callid,
            msg.from.tag.as_ref().ok_or(anyhow!("no from tag"))?
        ));

        let mut remote_uri = msg
            .remote_uri
            .clone()
            .ok_or(anyhow!("msg doens't have remote_uri"))?;
        remote_uri.is_registration = Some(true);

        let to_uri = msg.to.uri.clone();
        let contact = msg
            .contact
            .clone()
            .ok_or(anyhow!("msg doens't have contact"))?;
        let from_user = msg
            .from
            .uri
            .user
            .as_ref()
            .ok_or(anyhow!("no from user"))?
            .to_lowercase();
        let from_user = PRPOXY_SERVICE
            .db
            .get_user_by_name(&from_user)
            .await
            .ok_or(anyhow!("don't have user"))?;
        let exten = to_uri.user.as_ref().ok_or(anyhow!("no user in to uri"))?;
        let (presentity_id, presentity_kind) =
            Self::presentity_info(&from_user, exten).await?;

        let event_kind = match msg
            .event
            .clone()
            .unwrap_or("".to_string())
            .to_lowercase()
            .as_str()
        {
            "presence" => EventKind::Presence,
            "dialog" => EventKind::Dialog,
            _ => Err(anyhow!("event kind not supported"))?,
        };

        Ok(Watcher {
            watcher_id,
            presentity_id,
            presentity_kind,
            presentity_user: to_uri.user.clone().unwrap_or("".to_string()),
            presentity_domain: to_uri.host.clone(),
            event_kind,
            from: msg.from.clone(),
            to: msg.to.clone(),
            contact,
            remote_uri,
            callid: msg.callid.clone(),
        })
    }

    async fn presentity_info(
        from_user: &User,
        exten: &str,
    ) -> Result<(String, PrensentityKind)> {
        if let Some(exten) = exten.strip_prefix('*') {
            let shortcode = PRPOXY_SERVICE
                .db
                .get_shortcode(&from_user.tenant_id, exten)
                .await
                .ok_or(anyhow!("doens't have shortcode"))?;
            let value: serde_json::Value = serde_json::from_str(&shortcode.feature)?;
            let feature = value
                .get("feature")
                .ok_or(anyhow!("shortcode doesn't have feature"))?
                .as_str()
                .ok_or(anyhow!("shortcode feature is not string"))?;
            match feature.to_lowercase().as_str() {
                "parkretrieve" => {
                    let slot = value
                        .get("slot")
                        .ok_or(anyhow!("shortcode doesn't have feature"))?;
                    return Ok((
                        format!("{}_{}", slot, from_user.tenant_id),
                        PrensentityKind::ParkingSlot,
                    ));
                }
                "togglequeueavailable" => {
                    return Ok((
                        format!("{}:queue_availability", from_user.uuid),
                        PrensentityKind::QueueAvailability,
                    ));
                }
                "togglequeuelogin" => {
                    return Ok((
                        format!("{}:queue_login_logout", from_user.uuid),
                        PrensentityKind::QueueLoginLogout,
                    ));
                }
                _ => Err(anyhow!("don't suppport this shortcode"))?,
            }
        }

        if let Some(req_user) = PRPOXY_SERVICE.db.get_user_by_name(exten).await {
            if req_user.tenant_id == from_user.tenant_id {
                return Ok((req_user.uuid.clone(), PrensentityKind::User));
            }
        }

        if let Ok(number) = exten.parse::<i64>() {
            if let Some(extension) = PRPOXY_SERVICE
                .db
                .get_extension(&from_user.tenant_id, number)
                .await
            {
                if extension.discriminator == "sipuser" {
                    if let Some(user) = PRPOXY_SERVICE
                        .db
                        .get_user(&from_user.tenant_id, &extension.parent_id)
                        .await
                    {
                        return Ok((user.uuid, PrensentityKind::User));
                    }
                } else if extension.discriminator == "voicemailmenu" {
                    let menu = PRPOXY_SERVICE
                        .db
                        .get_voicemail_menu(
                            &extension.tenant_id,
                            &extension.parent_id,
                        )
                        .await
                        .ok_or(anyhow!("no voicemail menu"))?;
                    let mailbox = if menu.mailbox == "personal" {
                        from_user.personal_vmbox.clone().unwrap_or("".to_string())
                    } else {
                        menu.mailbox.to_string()
                    };
                    let vm_user = PRPOXY_SERVICE
                        .db
                        .get_voicemail_user(&extension.tenant_id, &mailbox)
                        .await
                        .ok_or(anyhow!("no mailbox found"))?;
                    return Ok((
                        format!("vm:{}", vm_user.uuid),
                        PrensentityKind::Mailbox,
                    ));
                }
            } else if let Some((number, dst_tenant)) = PRPOXY_SERVICE
                .db
                .get_tenant_extensions(&from_user.tenant_id)
                .await
                .and_then(|tenant_extenions| {
                    for extension in tenant_extenions {
                        if let Some(user_exten) =
                            exten.strip_prefix(&extension.number.to_string())
                        {
                            // Checking for empty number here
                            if let Ok(number) = user_exten.parse::<i64>() {
                                return Some((number, extension.dst_tenant_id));
                            }
                        }
                    }
                    None
                })
            {
                if let Some(extension) = PRPOXY_SERVICE
                    .db
                    .get_extension(&dst_tenant, number)
                    .await
                    .filter(|ext| ext.discriminator == "sipuser")
                {
                    if let Some(user) = PRPOXY_SERVICE
                        .db
                        .get_user(&dst_tenant, &extension.parent_id)
                        .await
                    {
                        return Ok((user.uuid, PrensentityKind::User));
                    }
                }
            }
        }

        Err(anyhow!("can't find presentity uri"))
    }

    pub async fn notify(&self) -> Result<()> {
        let key = format!("nebula:watcher:{}", self.watcher_id);
        let cseq = REDIS.hincrby_key_exits(&key, CSEQ).await? as i32;

        let mut msg = Message::default();
        msg.method = Some(Method::NOTIFY);
        msg.request_uri = Some(self.contact.uri.clone());
        msg.callid = self.callid.clone();
        msg.cseq = Cseq {
            seq: cseq,
            method: Method::NOTIFY,
        };
        msg.from = self.to.clone();
        msg.to = self.from.clone();
        msg.event = Some(self.event_kind.to_string());
        let expires = REDIS.ttl(&key).await.unwrap_or(0) as i32;
        msg.expires = Some(expires);
        msg.subscription_state = Some(format!("active;expires={}", expires));
        msg.content_type = Some(self.event_kind.content_type());
        msg.body = Some(match self.event_kind {
            EventKind::Presence => self.presence_body().await,
            EventKind::Dialog => self.dialog_body(cseq - 1).await,
        });
        msg.remote_uri = Some(self.remote_uri.clone());
        TM.send(&msg).await?;
        Ok(())
    }

    async fn dialog_body(&self, version: i32) -> String {
        let dialog = match &self.presentity_kind {
            PrensentityKind::User => {
                let dialogs = self
                    .user_dialogs(&self.presentity_id)
                    .await
                    .unwrap_or(Vec::new());
                let global_dnd = PRPOXY_SERVICE
                    .db
                    .get_user("admin", &self.presentity_id)
                    .await
                    .and_then(|u| u.global_dnd);
                if let Some(dialog) =
                    dialogs.iter().find(|d| d.state == DialogState::Confirmed)
                {
                    Some(dialog.clone())
                } else if dialogs.is_empty() && global_dnd == Some(true) {
                    Some(DialogInfo {
                        local: "DND".to_string(),
                        remote: "DND".to_string(),
                        direction: CallDirection::Recipient,
                        callid: nebula_utils::uuid(),
                        state: DialogState::Confirmed,
                        call_queue_id: None,
                        call_queue_name: None,
                        time: Utc::now(),
                    })
                } else {
                    dialogs.first().cloned()
                }
            }
            PrensentityKind::QueueAvailability => {
                let user_uuid = self.presentity_id.split(':').next();
                match user_uuid {
                    Some(user_uuid) => {
                        match PRPOXY_SERVICE.db.get_user("admin", user_uuid).await {
                            Some(user) => {
                                if user.available {
                                    None
                                } else {
                                    Some(DialogInfo {
                                        local: user.name.clone(),
                                        remote: user.name.clone(),
                                        direction: CallDirection::Initiator,
                                        callid: nebula_utils::uuid(),
                                        state: DialogState::Confirmed,
                                        call_queue_id: None,
                                        call_queue_name: None,
                                        time: Utc::now(),
                                    })
                                }
                            }
                            None => None,
                        }
                    }
                    None => None,
                }
            }
            PrensentityKind::QueueLoginLogout => {
                let user_uuid = self.presentity_id.split(':').next();
                match user_uuid {
                    Some(user_uuid) => {
                        match PRPOXY_SERVICE.db.get_user("admin", user_uuid).await {
                            Some(user) => {
                                if user.is_queue_login().await {
                                    None
                                } else {
                                    Some(DialogInfo {
                                        local: user.name.clone(),
                                        remote: user.name.clone(),
                                        direction: CallDirection::Initiator,
                                        callid: nebula_utils::uuid(),
                                        state: DialogState::Confirmed,
                                        call_queue_id: None,
                                        call_queue_name: None,
                                        time: Utc::now(),
                                    })
                                }
                            }
                            None => None,
                        }
                    }
                    None => None,
                }
            }
            PrensentityKind::ParkingSlot => {
                match self.parking_slot_presence(&self.presentity_id).await {
                    Err(_) => None,
                    Ok(details) => Some(DialogInfo {
                        local: details
                            .get("from")
                            .unwrap_or(&"".to_string())
                            .to_string(),
                        remote: details
                            .get("to")
                            .unwrap_or(&"".to_string())
                            .to_string(),
                        direction: details
                            .get("direction")
                            .and_then(|d| CallDirection::from_str(d).ok())
                            .unwrap_or(CallDirection::Initiator),
                        callid: details
                            .get("uuid")
                            .unwrap_or(&"".to_string())
                            .to_string(),
                        state: DialogState::Confirmed,
                        call_queue_id: None,
                        call_queue_name: None,
                        time: details
                            .get("time")
                            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                            .map(|t| t.with_timezone(&Utc))
                            .unwrap_or(Utc::now()),
                    }),
                }
            }
            PrensentityKind::Mailbox => {
                let msgs = PRPOXY_SERVICE
                    .db
                    .get_voicemail_messages(
                        self.presentity_id
                            .strip_prefix("vm:")
                            .unwrap_or(&self.presentity_id),
                    )
                    .await;
                match msgs {
                    None => None,
                    Some(msgs) => {
                        let new = msgs.iter().filter(|m| m.folder == "new").count();
                        let old =
                            msgs.iter().filter(|m| m.folder == "saved").count();
                        if new > 0 {
                            Some(DialogInfo {
                                local: new.to_string(),
                                remote: old.to_string(),
                                direction: CallDirection::Recipient,
                                state: DialogState::Confirmed,
                                callid: nebula_utils::uuid(),
                                call_queue_id: None,
                                call_queue_name: None,
                                time: Utc::now(),
                            })
                        } else {
                            None
                        }
                    }
                }
            }
        };
        let dialog = if let Some(dialog) = dialog {
            format!(
                r#"<dialog id="{}" call-id="{}" direction="{}"><state>{}</state><remote><identity>sip:{}@{}</identity><target uri="sip:{}@{}" /></remote><local><identity>sip:{}@{}</identity><target uri="sip:{}@{}" /></local></dialog>"#,
                dialog.callid,
                dialog.callid,
                dialog.direction.to_string(),
                dialog.state.to_string(),
                dialog.remote,
                self.presentity_domain,
                dialog.remote,
                self.presentity_domain,
                dialog.local,
                self.presentity_domain,
                dialog.local,
                self.presentity_domain,
            )
        } else {
            "".to_string()
        };
        let body = format!(
            r#"<?xml version="1.0"?><dialog-info xmlns="urn:ietf:params:xml:ns:dialog-info" version="{}" state="full" entity="sip:{}@{}">{}</dialog-info>"#,
            version, self.presentity_user, self.presentity_domain, dialog,
        );
        body
    }

    async fn user_dialogs(&self, user_uuid: &str) -> Result<Vec<DialogInfo>> {
        let user = PRPOXY_SERVICE
            .db
            .get_user("admin", user_uuid)
            .await
            .ok_or_else(|| anyhow!("can't find user"))?;
        let switchboard_host = get_tenant_switchboard_host(&user.tenant_id).await;
        let dialogs = PRPOXY_SERVICE
            .switchboard_rpc
            .client
            .get(format!(
                "http://{switchboard_host}/v1/users/{user_uuid}/dialogs"
            ))
            .send()
            .await?
            .json::<Vec<DialogInfo>>()
            .await?;
        Ok(dialogs)
    }

    async fn parking_slot_presence(
        &self,
        slot: &str,
    ) -> Result<HashMap<String, String>> {
        let mut splits = slot.split('_');
        splits.next();
        let tenant_id = splits.next().ok_or(anyhow!("invalid slot"))?;
        let switchboard_host = get_tenant_switchboard_host(tenant_id).await;
        let presence = PRPOXY_SERVICE
            .switchboard_rpc
            .client
            .get(format!("http://{switchboard_host}/v1/parking_slot/{slot}"))
            .send()
            .await?
            .json::<HashMap<String, String>>()
            .await?;
        Ok(presence)
    }

    async fn presence_body(&self) -> String {
        let (basic, note) = match &self.presentity_kind {
            PrensentityKind::User => {
                let dialogs = self
                    .user_dialogs(&self.presentity_id)
                    .await
                    .unwrap_or(Vec::new());
                let global_dnd = PRPOXY_SERVICE
                    .db
                    .get_user("admin", &self.presentity_id)
                    .await
                    .and_then(|u| u.global_dnd);
                let note =
                    if dialogs.iter().any(|d| d.state == DialogState::Confirmed) {
                        "On the phone"
                    } else if !dialogs.is_empty() {
                        "Proceeding"
                    } else if global_dnd == Some(true) {
                        "DND"
                    } else {
                        ""
                    };
                let basic = if let Some(user) = PRPOXY_SERVICE
                    .db
                    .get_user("admin", &self.presentity_id)
                    .await
                {
                    if user.has_locations().await {
                        "open"
                    } else {
                        "closed"
                    }
                } else {
                    "closed"
                };
                (basic, note.to_string())
            }
            PrensentityKind::ParkingSlot => {
                let note = if self
                    .parking_slot_presence(&self.presentity_id)
                    .await
                    .is_ok()
                {
                    "On the phone"
                } else {
                    ""
                };
                ("open", note.to_string())
            }
            PrensentityKind::QueueAvailability => {
                let mut note = "";
                let user_uuid = self.presentity_id.split(':').next();
                if let Some(user_uuid) = user_uuid {
                    if let Some(user) =
                        PRPOXY_SERVICE.db.get_user("admin", user_uuid).await
                    {
                        if !user.available {
                            note = "On the phone"
                        }
                    }
                }
                ("open", note.to_string())
            }
            PrensentityKind::QueueLoginLogout => {
                let mut note = "";
                let user_uuid = self.presentity_id.split(':').next();
                if let Some(user_uuid) = user_uuid {
                    if let Some(user) =
                        PRPOXY_SERVICE.db.get_user("admin", user_uuid).await
                    {
                        if !user.is_queue_login().await {
                            note = "On the phone"
                        }
                    }
                }
                ("open", note.to_string())
            }
            PrensentityKind::Mailbox => {
                let mut note = "".to_string();
                if let Some(msgs) = PRPOXY_SERVICE
                    .db
                    .get_voicemail_messages(
                        self.presentity_id
                            .strip_prefix("vm:")
                            .unwrap_or(&self.presentity_id),
                    )
                    .await
                {
                    let new = msgs.iter().filter(|m| m.folder == "new").count();
                    let old = msgs.iter().filter(|m| m.folder == "saved").count();
                    if new > 0 {
                        note = format!("{new}/{}", new + old);
                    }
                }
                ("open", note)
            }
        };
        let note = if note != "" {
            format!(
                r#"<note xmlns="urn:ietf:params:xml:ns:pidf">{}</note><dm:person xmlns:dm="urn:ietf:params:xml:ns:pidf:data-model" xmlns:rpid="urn:ietf:params:xml:ns:pidf:rpid" id="pers_mixingid"><rpid:activities><rpid:on-the-phone/></rpid:activities><dm:note>{}</dm:note></dm:person>"#,
                note, note
            )
        } else {
            "".to_string()
        };

        let body = format!(
            r#"<?xml version="1.0"?><presence xmlns="urn:ietf:params:xml:ns:pidf" entity="sip:{}@{}"><tuple xmlns="urn:ietf:params:xml:ns:pidf" id="tuple_mixingid"><status><basic>{}</basic></status></tuple>{}</presence>"#,
            self.presentity_user, self.presentity_domain, basic, note,
        );
        body
    }
}

impl Presence {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn notify_dialog(presentity_id: &str) -> Result<()> {
        let watchers_key = format!("nebula:watchers:{}", presentity_id);
        let watchers: Vec<String> = REDIS.smembers(&watchers_key).await?;
        for watcher in watchers {
            let watcher_key = &format!("nebula:watcher:{}", watcher);
            if !REDIS.exists(watcher_key).await.unwrap_or(false) {
                let _ = REDIS.srem(&watchers_key, &watcher).await;
                continue;
            }
            tokio::spawn(async move {
                let _ = Self::notify_watcher(&watcher).await;
            });
        }
        Ok(())
    }

    pub async fn notify_watcher(watcher_id: &str) -> Result<()> {
        let watcher: String = REDIS
            .hget(&format!("nebula:watcher:{}", watcher_id), "watcher")
            .await?;
        let watcher: Watcher = serde_json::from_str(&watcher)?;
        watcher.notify().await?;
        Ok(())
    }

    pub async fn subscribe(&self, msg: &Message) -> Result<Watcher> {
        let watcher = Watcher::from_msg(msg).await?;

        let expires = msg.expires.unwrap_or(0);

        if let Some(dialog) = msg.dialog_id() {
            if expires == 0 {
                let _ = REDIS.del(&format!("nebula:dialog:{dialog}")).await;
            } else {
                let _ = REDIS
                    .setex(
                        &format!("nebula:dialog:{dialog}"),
                        expires as u64,
                        &msg.callid,
                    )
                    .await;
            }
        }

        if expires == 0 {
            let _ = REDIS
                .del(&format!("nebula:watcher:{}", watcher.watcher_id))
                .await;
            let _ = REDIS
                .srem(
                    &format!("nebula:watchers:{}", watcher.presentity_id),
                    &watcher.watcher_id,
                )
                .await;
            return Ok(watcher);
        }

        let _ = REDIS
            .sadd(
                &format!("nebula:watchers:{}", watcher.presentity_id),
                &watcher.watcher_id,
            )
            .await;
        let _ = REDIS
            .expire(&format!("nebula:watchers:{}", watcher.presentity_id), 86400)
            .await;
        let _ = REDIS
            .hset(
                &format!("nebula:watcher:{}", watcher.watcher_id),
                "watcher",
                &serde_json::to_string(&watcher)?,
            )
            .await;
        let _ = REDIS
            .expire(
                &format!("nebula:watcher:{}", watcher.watcher_id),
                expires as u64,
            )
            .await;

        Ok(watcher)
    }
}
