use crate::api::{is_channel_answered, user_dialogs};
use crate::auxiliary::{format_aux_records, AuxRecord};
use crate::message::{
    DialogState, UserPresence, UserPresenceDetails, UserRegistration,
};
use crate::schema::{
    cdr_events, cdr_items, faxes, login_as_users, numbers, queue_failed_users,
    queue_records, sipuser_aux_log, sipuser_queue_login_log, sounds,
    voicemail_messages,
};
use crate::usrloc::{
    CALL_ID, CONTACT, EXPIRES_AT, LOCATION_ID, RECEIVED, REGISTER_AT, USER_AGENT,
};
use crate::{api::DB, usrloc::Usrloc};
use anyhow::{anyhow, Result};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use chrono_tz::{Tz, UTC};
use diesel::prelude::*;
use diesel::{QueryDsl, Queryable};
use futures::stream::{self, StreamExt};
use nebula_redis::REDIS;
use phonenumber::PhoneNumber;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::SystemTime;
use tokio::task::spawn_blocking;
use uuid::Uuid;

pub trait CacheKey {
    fn cache_keys(&self) -> Vec<String>;
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone, Default)]
pub struct User {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub domain: Option<String>,
    pub tenant_id: String,
    pub extension: Option<i64>,
    pub name_lower: String,
    pub deleted: i64,
    pub display_name: Option<String>,
    pub caller_id: Option<String>,
    pub password: String,
    pub music_on_hold: Option<String>,
    pub recording: Option<bool>,
    pub unlimited_recording: Option<bool>,
    pub require_auth: bool,
    pub duration: Option<i64>,
    pub transfer_mailbox: bool,
    pub transfer_show_sipuser: bool,
    pub transfer_backup_extension: Option<i64>,
    pub available: bool,
    pub trunk: bool,
    pub pickup: Option<bool>,
    pub be_picked_up: Option<bool>,
    pub listen: Option<bool>,
    pub whisper: Option<bool>,
    pub barge: Option<bool>,
    pub login: Option<bool>,
    pub be_listened: Option<bool>,
    pub be_whispered: Option<bool>,
    pub be_barged: Option<bool>,
    pub be_login: Option<bool>,
    pub encryption: Option<bool>,
    pub video: Option<bool>,
    pub cc: Option<String>,
    pub personal_vmbox: Option<String>,
    pub device: Option<String>,
    pub device_token: Option<String>,
    pub chat_device_token: Option<String>,
    pub mute_chat_notification: bool,
    pub force_chat_notification: bool,
    pub call_handling: Option<String>,
    pub timezone: Option<String>,
    pub msteam: Option<String>,
    pub msteam_domain: Option<String>,
    pub disable_call_waiting: bool,
    pub alert_info_patterns: Option<String>,
    pub pin: Option<String>,
    pub hot_desk_user: Option<bool>,
    pub aux_schedule: Option<String>,
    pub aux_schedule_timezone: Option<String>,
    pub queue_answer_wait: Option<i64>,
    pub queue_no_answer_wait: Option<i64>,
    pub queue_reject_wait: Option<i64>,
    pub disable_external_transfer_recording: Option<bool>,
    pub global_dnd: Option<bool>,
    pub global_forward: Option<String>,
}

impl CacheKey for User {
    fn cache_keys(&self) -> Vec<String> {
        let msteam = self.msteam.clone().unwrap_or_default();
        let msteam_domain = self.msteam_domain.clone().unwrap_or_default();

        let tz = timezone_tz(self.aux_schedule_timezone.as_deref());
        let now = Utc::now().with_timezone(&tz);
        let today = now.date().to_string();

        vec![
            format!("nebula:cache:user:name:{}", self.name.to_lowercase()),
            format!("nebula:cache:user:uuid:{}", self.uuid),
            format!("nebula:cache:trunk:user:{}", self.uuid),
            format!("nebula:cache:user:msteam:{msteam}:{msteam_domain}"),
            format!("nebula:cache:user:groups:{}", self.uuid),
            format!(
                "nebula:cache:user:{}:aux_code_schedule:{today}:{}",
                self.uuid,
                self.aux_schedule_timezone.as_deref().unwrap_or("")
            ),
        ]
    }
}

impl User {
    pub async fn channels(&self) -> Result<Vec<String>> {
        let channels: Vec<String> = REDIS
            .smembers(&format!("nebula:sipuser:{}:channels", self.uuid))
            .await?;
        Ok(channels)
    }

    pub async fn num_channels(&self) -> Result<usize> {
        REDIS
            .scard(&format!("nebula:sipuser:{}:channels", self.uuid))
            .await
    }

    pub async fn is_available(&self) -> Result<bool> {
        let user = DB
            .get_user(&self.tenant_id, &self.uuid)
            .await
            .ok_or_else(|| anyhow!("no user {}", self.uuid))?;
        Ok(user.available)
    }

    async fn last_queue_login(&self) -> Option<SipuserQueueLoginLog> {
        let key = SipuserQueueLoginLog::parse_cache_key(&self.uuid, None);
        if let Some(cache) = DB.get_cache::<SipuserQueueLoginLog>(&key).await {
            return cache;
        }

        let user_id = self.uuid.clone();
        let pool = DB.pool.clone();
        let login = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sipuser_queue_login_log::table
                .filter(sipuser_queue_login_log::user_id.eq(user_id))
                .filter(sipuser_queue_login_log::queue_id.is_null())
                .order_by(sipuser_queue_login_log::time.desc())
                .first::<SipuserQueueLoginLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        DB.put_cache(&key, login.as_ref()).await;
        login
    }

    pub async fn last_individual_queue_login(
        &self,
        queue_id: &str,
    ) -> Option<SipuserQueueLoginLog> {
        let key = SipuserQueueLoginLog::parse_cache_key(&self.uuid, Some(queue_id));
        if let Some(cache) = DB.get_cache::<SipuserQueueLoginLog>(&key).await {
            return cache;
        }

        let user_id = self.uuid.clone();
        let queue_id = queue_id.to_string();
        let pool = DB.pool.clone();
        let login = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sipuser_queue_login_log::table
                .filter(sipuser_queue_login_log::user_id.eq(user_id))
                .filter(sipuser_queue_login_log::queue_id.eq(queue_id))
                .order_by(sipuser_queue_login_log::time.desc())
                .first::<SipuserQueueLoginLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        DB.put_cache(&key, login.as_ref()).await;
        login
    }

    async fn last_aux_code(&self) -> Option<SipuserAuxLog> {
        let key = format!("nebula:cache:user:{}:last_aux_code", self.uuid,);
        if let Some(cache) = DB.get_cache::<SipuserAuxLog>(&key).await {
            return cache;
        }

        let user_id = self.uuid.clone();
        let pool = DB.pool.clone();
        let code = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sipuser_aux_log::table
                .filter(sipuser_aux_log::user_id.eq(user_id))
                .order_by(sipuser_aux_log::time.desc())
                .first::<SipuserAuxLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        DB.put_cache(&key, code.as_ref()).await;
        code
    }

    pub async fn is_aux_available(&self) -> bool {
        let code = self.current_aux_code().await;
        if let Some(code) = code {
            if code.code.trim().to_lowercase() != "available" {
                return false;
            }
        }
        true
    }

    pub async fn is_queue_login(&self) -> bool {
        let login = self.last_queue_login().await;
        if let Some(login) = login {
            return login.login;
        }
        true
    }

    pub async fn is_individual_queue_login(&self, queue_id: &str) -> bool {
        let login = self.last_individual_queue_login(queue_id).await;
        if let Some(login) = login {
            return login.login;
        }
        true
    }

    pub async fn current_aux_code(&self) -> Option<SipuserAuxLog> {
        let last = self.last_aux_code().await;
        if let Some(schedule_today) = self.aux_code_schedule_today().await {
            let now = Utc::now();
            let mut current_code = None;
            for (t, code) in schedule_today {
                if t <= now
                    && last.as_ref().map(|last| last.time < t).unwrap_or(true)
                {
                    let user_id = self.uuid.clone();
                    current_code = Some((code.clone(), t));
                    tokio::spawn(async move {
                        let _ = DB
                            .insert_sipuser_aux_log(
                                user_id,
                                t,
                                code,
                                "schedule".to_string(),
                            )
                            .await;
                    });
                }
            }
            if let Some((code, t)) = current_code {
                return Some(SipuserAuxLog {
                    id: 0,
                    user_id: self.uuid.clone(),
                    time: t,
                    code,
                    set_by: "schedule".to_string(),
                });
            }
        }
        last
    }

    async fn aux_code_schedule_today(&self) -> Option<Vec<(DateTime<Utc>, String)>> {
        let schedule = self.aux_schedule.as_deref().unwrap_or("");
        if schedule.is_empty() {
            return None;
        }

        let tz = timezone_tz(self.aux_schedule_timezone.as_deref());
        let now = Utc::now().with_timezone(&tz);
        let today = now.date().to_string();

        let key = format!(
            "nebula:cache:user:{}:aux_code_schedule:{today}:{}",
            self.uuid,
            self.aux_schedule_timezone.as_deref().unwrap_or("")
        );
        if let Some(cache) = DB.get_cache::<Vec<(DateTime<Utc>, String)>>(&key).await
        {
            return cache;
        }

        let records: Vec<AuxRecord> = serde_json::from_str(schedule).ok()?;
        let records = format_aux_records(&now, &records);
        let records: Vec<(DateTime<Utc>, String)> = records
            .iter()
            .map(|(t, s)| (t.with_timezone(&Utc), s.to_string()))
            .collect();
        DB.put_cache::<Vec<(DateTime<Utc>, String)>>(&key, Some(&records))
            .await;

        Some(records)
    }

    pub async fn registrations(&self) -> Vec<UserRegistration> {
        let username = &self.name.to_lowercase();
        let location_key = &format!("nebula:location:{username}");
        let hashed_callids: Vec<String> = REDIS
            .lrange(location_key, 0, -1)
            .await
            .unwrap_or(Vec::new());
        let registrations: Vec<UserRegistration> = stream::iter(hashed_callids)
            .filter_map(|hashed_callid| async move {
                let contact_map_result: Result<HashMap<String, String>, _> = REDIS
                    .hgetall(&format!("nebula:location:{username}:{hashed_callid}"))
                    .await;
                if let Ok(contact_map) = contact_map_result {
                    if !contact_map.is_empty() {
                        return Some(UserRegistration {
                            username: self.name.clone(),
                            cc: "GB".to_string(),
                            contact: contact_map.get(CONTACT)?.to_string(),
                            received: contact_map.get(RECEIVED)?.to_string(),
                            registered_at: contact_map.get(REGISTER_AT)?.to_string(),
                            expires: contact_map.get(EXPIRES_AT)?.to_string(),
                            user_agent: contact_map.get(USER_AGENT)?.to_string(),
                            callid: contact_map.get(CALL_ID)?.to_string(),
                            location_id: contact_map.get(LOCATION_ID)?.to_string(),
                            dc: "London".to_string(),
                            proxy: "Nebula".to_string(),
                            dnd: contact_map
                                .get("dnd")
                                .map(|dnd| dnd == "yes")
                                .unwrap_or(false),
                            dnd_allow_internal: contact_map
                                .get("dnd_allow_internal")
                                .map(|dnd_allow_internal| {
                                    dnd_allow_internal == "yes"
                                })
                                .unwrap_or(false),
                        });
                    }
                }

                let _ = REDIS.lrem(location_key, 0, &hashed_callid).await;
                None
            })
            .collect()
            .await;
        registrations
    }

    pub async fn queue_status(&self, queue_id: &str) -> String {
        let presence = self.presence().await;
        if presence.status != "open" && presence.status != "closed" {
            return presence.status.clone();
        }

        if !self.is_available().await.unwrap_or(false)
            || !presence.queue_login
            || !self.is_individual_queue_login(queue_id).await
            || presence
                .aux_code
                .is_some_and(|log| !log.code.to_lowercase().eq("available"))
        {
            return "unavailable".to_string();
        }

        if let Some(code) = self.current_aux_code().await {
            if code.code.trim().to_lowercase() != "available" {
                return code.code;
            }
        }

        let base_key = format!("nebula:callqueue:{}:user:{}", queue_id, self.uuid);
        let noanswer_wait = format!("{}:{}", base_key, "no_answer_wait");
        let answer_wait = format!("{}:{}", base_key, "answer_wait");
        let reject_wait = format!("{}:{}", base_key, "reject_wait");
        if REDIS.exists(&noanswer_wait).await.unwrap_or(false) {
            return "no_answer_wait".to_string();
        }
        if REDIS.exists(&answer_wait).await.unwrap_or(false) {
            return "answer_wait".to_string();
        }
        if REDIS.exists(&reject_wait).await.unwrap_or(false) {
            return "reject_wait".to_string();
        }

        if presence.status == "closed" {
            return "closed".to_string();
        }

        "available".to_string()
    }

    pub async fn presence(&self) -> UserPresence {
        let dialogs = user_dialogs(&self.uuid).await.unwrap_or(Vec::new());
        let (status, details, time) =
            match dialogs.iter().find(|d| d.state == DialogState::Confirmed) {
                Some(d) => (
                    "confirmed",
                    Some(UserPresenceDetails {
                        local: d.local.clone(),
                        remote: d.remote.clone(),
                        direction: d.direction.to_string(),
                        call_queue_id: d.call_queue_id.clone(),
                        call_queue_name: d.call_queue_name.clone(),
                    }),
                    Some(d.time),
                ),
                None => match dialogs.get(0) {
                    Some(d) => (
                        "early",
                        Some(UserPresenceDetails {
                            local: d.local.clone(),
                            remote: d.remote.clone(),
                            direction: d.direction.to_string(),
                            call_queue_id: d.call_queue_id.clone(),
                            call_queue_name: d.call_queue_name.clone(),
                        }),
                        Some(d.time),
                    ),
                    None => match self.has_locations().await {
                        true => ("open", None, None),
                        false => ("closed", None, None),
                    },
                },
            };

        let apps = DB.get_mobile_apps(self).await.unwrap_or(Vec::new());
        let app = match !apps.is_empty() {
            true => {
                let mut available = false;
                for app in apps.iter() {
                    if !self
                        .is_app_unavailable(&app.device, &app.device_token)
                        .await
                    {
                        available = true;
                        break;
                    }
                }
                let app_status = if available {
                    "available".to_string()
                } else {
                    "unavailable".to_string()
                };

                let dnd = apps.iter().all(|a| a.dnd);
                json!({
                    "status": app_status,
                    "dnd": dnd,
                })
            }
            false => json!({}),
        };

        let code = self.current_aux_code().await;

        UserPresence {
            sipuser: self.name.to_lowercase(),
            status: status.to_string(),
            time: time.map(|t| t.timestamp()).unwrap_or(0).to_string(),
            app,
            queue_available: self.is_available().await.unwrap_or(false),
            queue_login: self.is_queue_login().await,
            global_dnd: self.global_dnd.unwrap_or(false),
            aux_code: code.map(|code| AuxCodeResponse {
                set_by: code.set_by,
                code: code.code,
                time: code.time.to_rfc3339(),
            }),
            registrations: REDIS
                .lrange(
                    &format!("nebula:location:{}", self.name.to_lowercase()),
                    0,
                    -1,
                )
                .await
                .unwrap_or(Vec::new()),
            details: details
                .map(|d| serde_json::to_value(d).unwrap())
                .unwrap_or(json!({})),
        }
    }

    pub async fn has_locations(&self) -> bool {
        if !Usrloc::lookup(self).await.unwrap_or_default().is_empty() {
            return true;
        }

        if !DB
            .get_mobile_apps(self)
            .await
            .unwrap_or_default()
            .is_empty()
        {
            return true;
        }

        let msteam = self.msteam.clone().unwrap_or_default();
        let msteam_domain = self.msteam_domain.clone().unwrap_or_default();
        if !msteam.is_empty() && !msteam_domain.is_empty() {
            return true;
        }

        false
    }

    pub fn call_handling(&self) -> Option<UserCallHandling> {
        self.call_handling
            .as_ref()
            .and_then(|h| serde_json::from_str(h).ok())
    }

    pub async fn update_app_status(
        &self,
        device: &str,
        device_token: &str,
        status: &str,
    ) {
        let _ = REDIS
            .set(
                &format!(
                    "nebula:user:{}:{}:{}:app_status",
                    self.uuid, device, device_token
                ),
                status,
            )
            .await;
    }

    pub async fn is_talking(&self) -> bool {
        for channel in self.channels().await.unwrap_or_default() {
            if is_channel_answered(&channel).await {
                return true;
            }
        }
        false
    }

    pub async fn is_app_unavailable(
        &self,
        device: &str,
        device_token: &str,
    ) -> bool {
        REDIS
            .get(&format!(
                "nebula:user:{}:{}:{}:app_status",
                self.uuid, device, device_token
            ))
            .await
            .unwrap_or_else(|_| "".to_string())
            == "unavailable"
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct UserCallHandling {
    pub forward_internal: Option<bool>,
    pub forward_internal_number: Option<String>,
    pub forward_external: Option<bool>,
    pub forward_external_number: Option<String>,
    pub blind_transfer_return_to_sender: Option<bool>,
    pub busy_callflow: Option<serde_json::Value>,
    pub noanswer_callflow: Option<serde_json::Value>,
    pub unavailable_callflow: Option<serde_json::Value>,
    pub busy_callflow_id: Option<String>,
    pub noanswer_callflow_id: Option<String>,
    pub unavailable_callflow_id: Option<String>,
    pub hide_callflow_name: Option<bool>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Group {
    pub uuid: String,
    pub name: String,
    pub tenant_id: String,
    pub members: Vec<User>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct HuntGroup {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub tenant_id: String,
    #[serde(deserialize_with = "ok_or_default")]
    pub updated_at: Option<SystemTime>,
    deleted: i64,
}

impl CacheKey for HuntGroup {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:hunt_group:{}", self.uuid),
            format!("nebula:cache:hunt_group:{}:members", self.uuid),
        ]
    }
}

fn ok_or_default<'a, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'a> + Default,
    D: Deserializer<'a>,
{
    let v: Value = Deserialize::deserialize(deserializer)?;
    Ok(T::deserialize(v).unwrap_or_default())
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct GroupMember {
    pub id: i64,
    pub sip_user_uuid: String,
    pub hunt_group_uuid: String,
    deleted: i64,
}

impl CacheKey for GroupMember {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:user:groups:{}", self.sip_user_uuid),
            format!("nebula:cache:hunt_group:{}:members", self.hunt_group_uuid),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct Extension {
    pub id: i64,
    pub number: i64,
    pub tenant_id: String,
    pub deleted: i64,
    pub discriminator: String,
    pub parent_id: String,
}

impl CacheKey for Extension {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!(
            "nebula:cache:extension:{}:{}",
            self.tenant_id, self.number
        )]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct BlackList {
    pub id: i64,
    pub tenant_id: String,
    deleted: i64,
    pub patterns: Option<String>,
}

impl CacheKey for BlackList {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:black_list:{}", self.tenant_id)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct Number {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: Option<String>,
    pub number: String,
    pub country_code: String,
    pub e164: String,
    pub deleted: i64,
    pub call_flow_id: Option<String>,
    pub diary_id: Option<String>,
    pub require_auth: bool,
    pub limit: i64,
    pub sip_user_id: Option<String>,
    pub is_reseller: bool,
    pub ziron_tag: Option<String>,
    pub ddi_display_name_show_number: Option<bool>,
}

#[derive(AsChangeset, Default)]
#[table_name = "numbers"]
pub struct NumberChange {
    pub call_flow_id: Option<Option<String>>,
    pub diary_id: Option<Option<String>>,
}

impl CacheKey for Number {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:number:{}", self.e164),
            format!("nebula:cache:number:uuid:{}", self.uuid),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct CallFlow {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub flow: String,
    pub unlimited_recording: bool,
    pub shown_in_callerid: bool,
    pub show_original_callerid: bool,
    pub custom_callerid: Option<String>,
}

impl CacheKey for CallFlow {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:callflow:{}", self.uuid)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct ShortCode {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub shortcode: String,
    pub in_channel: bool,
    pub out_channel: bool,
    pub feature: String,
    pub no_star: Option<bool>,
}

impl CacheKey for ShortCode {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:shortcodes:{}", self.tenant_id),
            format!("nebula:cache:shortcodes:in_channel:{}", self.tenant_id),
            format!("nebula:cache:shortcodes:no_star:{}", self.tenant_id),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct MobileApp {
    pub uuid: Uuid,
    pub created_at: Option<DateTime<Utc>>,
    pub deleted: String,
    pub user: String,
    pub device: String,
    pub device_token: String,
    pub chat_device_token: String,
    pub dnd: bool,
    pub dnd_allow_internal: bool,
    pub mute_chat_notification: bool,
    pub force_chat_notification: bool,
}

impl CacheKey for MobileApp {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:user:uuid:{}:mobile_apps", self.user),
            format!(
                "nebula:cache:mobile_app:{}:{}",
                self.device, self.device_token
            ),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct Diary {
    pub uuid: Uuid,
    pub tenant_id: String,
    deleted: String,
    pub name: String,
    pub records: String,
    pub timezone: String,
}

impl CacheKey for Diary {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:diary:{}", self.uuid)]
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DiaryRecord {
    pub time: String,
    pub call_flow: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct SipuserPermission {
    pub uuid: Uuid,
    deleted: Option<String>,
    pub tenant_id: Option<String>,
    pub from_user: Option<String>,
    pub to_user: Option<String>,
    pub from_group: Option<String>,
    pub to_group: Option<String>,
    pub feature: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct SipuserTimeSchedule {
    pub uuid: Uuid,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
}

#[derive(Insertable, Queryable)]
#[table_name = "login_as_users"]
pub struct NewLoginAsUser {
    pub created_at: DateTime<Utc>,
    pub deleted: String,
    pub user: String,
    pub login_as: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct LoginAsUser {
    pub created_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub uuid: Uuid,
    pub deleted: String,
    pub user: String,
    pub login_as: String,
}

impl CacheKey for LoginAsUser {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:login_as_user:{}", self.login_as),
            format!("nebula:cache:login_from_user:{}", self.user),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct TenantExtension {
    pub uuid: Uuid,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub tenant_id: String,
    pub deleted: String,
    pub number: i64,
    pub dst_tenant_id: String,
}

impl CacheKey for TenantExtension {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:tenant_extensions:{}", self.tenant_id)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct SpeedDial {
    pub uuid: Uuid,
    deleted: String,
    pub tenant_id: String,
    pub code: String,
    pub number: String,
}

impl CacheKey for SpeedDial {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!(
            "nebula:cache:speed_dial:{}:{}",
            self.tenant_id, self.code
        )]
    }
}

#[derive(Queryable, Deserialize, Serialize, Clone, Debug)]
pub struct Trunk {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub user: String,
    pub domain: String,
    pub port: Option<i64>,
    pub transport: String,
    pub cc: Option<String>,
    pub caller_id: Option<String>,
    pub show_pai: Option<bool>,
    pub show_diversion: Option<bool>,
    pub pass_pai: Option<bool>,
    pub codecs: Option<String>,
    pub user_agent: Option<bool>,
    pub display_name_as_callerid: Option<bool>,
}

impl CacheKey for Trunk {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:trunk:{}", self.uuid)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Clone, Debug)]
pub struct TrunkAuthIp {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub ip: String,
    pub info: String,
}

impl CacheKey for TrunkAuthIp {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:trunk:ip:{}:{}", self.ip, self.info)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug)]
pub struct Recording {
    pub id: i64,
    pub tenant_id: String,
    deleted: i64,
    pub expires_in: i64,
    pub global_enabled: Option<bool>,
}

impl CacheKey for Recording {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:recording:{}", self.tenant_id)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Clone, Debug)]
pub struct TrunkAuth {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub trunk_uuid: String,
    pub discriminator: String,
    pub member_uuid: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct VoicemailUser {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub greeting: String,
    pub beep: bool,
    pub password: String,
    pub mailbox: String,
    pub play_timestamp: bool,
    pub play_callerid: bool,
    pub duration_limit: Option<i64>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct VoicemailMessage {
    pub id: i64,
    pub uuid: String,
    pub deleted: i64,
    pub deleted_at: Option<SystemTime>,
    pub voicemail_user_uuid: String,
    pub folder: String,
    pub callerid: String,
    pub calleeid: String,
    pub time: SystemTime,
    pub duration: i64,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct VoicemailMenu {
    pub id: i64,
    pub uuid: String,
    pub deleted: i64,
    pub tenant_id: String,
    pub extension: i64,
    pub mailbox: String,
    pub name: String,
}

#[derive(Insertable, Queryable)]
#[table_name = "voicemail_messages"]
pub struct NewVoicemailMessage {
    pub uuid: String,
    pub voicemail_user_uuid: String,
    pub deleted: i64,
    pub callerid: String,
    pub calleeid: String,
    pub folder: String,
    pub time: SystemTime,
    pub duration: i64,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct Sound {
    pub id: i64,
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub time: Option<DateTime<Utc>>,
    pub duration: f64,
}

impl CacheKey for Sound {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:sound:{}", self.uuid)]
    }
}

#[derive(Insertable, Queryable)]
#[table_name = "sounds"]
pub struct NewSound {
    pub uuid: String,
    pub tenant_id: String,
    pub deleted: i64,
    pub name: String,
    pub time: DateTime<Utc>,
    pub duration: f64,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct VoicemailMember {
    pub id: i64,
    deleted: i64,
    pub voicemail_user_uuid: String,
    pub discriminator: String,
    pub member_uuid: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct QueueGroup {
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub strategy: String,
    pub members: String,
    pub ring_progressively: bool,
    pub ring_timeout: i64,
    pub duration: i64,
    pub answer_wait: i64,
    pub no_answer_wait: i64,
    pub reject_wait: i64,
    // if hide_missed_calls is true, we will send "call completed elsewhere"
    // when sending cancel
    pub hide_missed_calls: Option<bool>,
}

impl CacheKey for QueueGroup {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:account_queue_groups:{}", self.tenant_id),
            format!("nebula:cache:queue_group:{}", self.uuid),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct QueueHint {
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub frequency: i64,
    pub audios: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct QueueHintAudio {
    pub wait_audio: Option<bool>,
    pub count: Option<i64>,
    pub delay: Option<u64>,
    pub waiting_time: Option<u64>,
    pub frequency: Option<u64>,
    pub files: Vec<String>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct MohClass {
    pub uuid: String,
    pub tenant_id: String,
    deleted: i64,
    pub name: String,
    pub random: bool,
}

impl CacheKey for MohClass {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:moh_class:{}", self.uuid)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct MohMusic {
    pub id: i64,
    deleted: i64,
    pub sound_uuid: String,
    pub moh_class_uuid: String,
}

impl CacheKey for MohMusic {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:moh_music:{}", self.moh_class_uuid)]
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MusicOnHold {
    pub play_id: String,
    pub random: bool,
    pub sounds: Vec<String>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone, Default)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    pub host: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub uri: Option<String>,
    pub location: Option<String>,
    pub realm: Option<String>,
}

impl CacheKey for Provider {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!(
                "nebula:cache:provider:{}",
                self.host.as_deref().unwrap_or("")
            ),
            format!(
                "nebula:cache:provider:realm:{}",
                self.realm.as_deref().unwrap_or("")
            ),
            format!(
                "nebula:cache:registration_provider:{}",
                self.location.as_deref().unwrap_or("")
            ),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct CdrItem {
    pub uuid: Uuid,
    pub tenant_id: String,
    pub start: DateTime<Utc>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub disposition: Option<String>,
    pub duration: Option<i64>,
    pub billsec: Option<i64>,
    pub talking_start: Option<DateTime<Utc>>,
    pub talking_end: Option<DateTime<Utc>>,
    pub talking_duration: Option<i64>,
    pub call_type: Option<String>,
    pub from_uuid: Option<String>,
    pub from_exten: Option<String>,
    pub from_type: Option<String>,
    pub from_user: Option<String>,
    pub from_name: Option<String>,
    pub to_uuid: Option<String>,
    pub to_exten: Option<String>,
    pub to_type: Option<String>,
    pub to_user: Option<String>,
    pub to_name: Option<String>,
    pub answer_uuid: Option<String>,
    pub answer_exten: Option<String>,
    pub answer_type: Option<String>,
    pub answer_user: Option<String>,
    pub answer_name: Option<String>,
    pub callflow_uuid: Option<String>,
    pub callflow_name: Option<String>,
    pub parent: Option<String>,
    pub child: Option<String>,
    pub recording: Option<String>,
    pub cost: Option<BigDecimal>,
    pub parent_cost: Option<BigDecimal>,
    pub queue_children: Option<String>,
    pub ziron_tag: Option<String>,
    pub hangup_direction: Option<String>,
    pub recording_uploaded: Option<bool>,
    pub caller_id: Option<String>,
    pub caller_id_name: Option<String>,
    pub operator_code: Option<String>,
}

#[derive(AsChangeset, Default, Clone, Debug)]
#[table_name = "cdr_items"]
pub struct CdrItemChange {
    pub start: Option<DateTime<Utc>>,
    pub tenant_id: Option<String>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub disposition: Option<String>,
    pub duration: Option<i64>,
    pub billsec: Option<i64>,
    pub talking_start: Option<DateTime<Utc>>,
    pub talking_end: Option<DateTime<Utc>>,
    pub talking_duration: Option<i64>,
    pub call_type: Option<String>,
    pub from_uuid: Option<String>,
    pub from_exten: Option<String>,
    pub from_type: Option<String>,
    pub from_user: Option<String>,
    pub from_name: Option<String>,
    pub to_uuid: Option<String>,
    pub to_exten: Option<String>,
    pub to_type: Option<String>,
    pub to_user: Option<String>,
    pub to_name: Option<String>,
    pub answer_uuid: Option<String>,
    pub answer_exten: Option<String>,
    pub answer_type: Option<String>,
    pub answer_user: Option<String>,
    pub answer_name: Option<String>,
    pub callflow_uuid: Option<String>,
    pub callflow_name: Option<String>,
    pub parent: Option<String>,
    pub child: Option<String>,
    pub recording: Option<String>,
    pub cost: Option<BigDecimal>,
    pub parent_cost: Option<BigDecimal>,
    pub queue_children: Option<String>,
    pub ziron_tag: Option<String>,
    pub hangup_direction: Option<String>,
    pub recording_uploaded: Option<bool>,
    pub caller_id: Option<String>,
    pub caller_id_name: Option<String>,
    pub operator_code: Option<String>,
}

#[derive(Insertable, Queryable)]
#[table_name = "cdr_items"]
pub struct NewCdrItem {
    pub start: DateTime<Utc>,
    pub disposition: String,
    pub tenant_id: String,
    pub cost: BigDecimal,
}

#[derive(Insertable, Queryable, Deserialize)]
#[table_name = "cdr_items"]
pub struct NewCdrItemFull {
    pub start: DateTime<Utc>,
    pub disposition: String,
    pub tenant_id: String,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
    pub billsec: Option<i64>,
    pub call_type: Option<String>,
    pub from_uuid: Option<String>,
    pub from_exten: Option<String>,
    pub from_type: Option<String>,
    pub from_user: Option<String>,
    pub from_name: Option<String>,
    pub to_uuid: Option<String>,
    pub to_exten: Option<String>,
    pub to_type: Option<String>,
    pub to_user: Option<String>,
    pub to_name: Option<String>,
    pub answer_uuid: Option<String>,
    pub answer_exten: Option<String>,
    pub answer_type: Option<String>,
    pub answer_user: Option<String>,
    pub answer_name: Option<String>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct CdrEvent {
    pub uuid: Uuid,
    pub cdr_id: Uuid,
    pub event_type: String,
    pub start: DateTime<Utc>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
    pub billsec: Option<i64>,
    pub user_id: Option<String>,
    pub bridge_id: Option<String>,
    pub user_agent: Option<String>,
    pub to_type: Option<String>,
    pub hangup_direction: Option<String>,
    pub caller_id: Option<String>,
    pub caller_id_name: Option<String>,
}

#[derive(Insertable, Queryable)]
#[table_name = "cdr_events"]
pub struct NewCdrEvent {
    pub cdr_id: Uuid,
    pub event_type: String,
    pub start: DateTime<Utc>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub duration: Option<i64>,
    pub billsec: Option<i64>,
    pub user_id: Option<String>,
    pub bridge_id: Option<String>,
    pub user_agent: Option<String>,
    pub to_type: Option<String>,
    pub hangup_direction: Option<String>,
    pub caller_id: Option<String>,
    pub caller_id_name: Option<String>,
}

#[derive(Queryable, Deserialize, Serialize, Debug)]
pub struct Room {
    pub uuid: Uuid,
    pub deleted: String,
    pub name: String,
    pub tenant_id: String,
    pub password: Option<String>,
    pub open: bool,
    pub force_password: bool,
    pub force_waiting_room: bool,
}

impl CacheKey for Room {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:room:{}", self.uuid)]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug)]
pub struct RoomMember {
    pub uuid: Uuid,
    pub deleted: String,
    pub user_id: String,
    pub room_id: String,
    pub admin: bool,
}

impl CacheKey for RoomMember {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!("nebula:cache:room:members:{}", self.room_id),
            format!("nebula:cache:room:member:{}:{}", self.room_id, self.user_id),
        ]
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct QueueRecord {
    pub uuid: Uuid,
    pub queue_id: String,
    pub start: DateTime<Utc>,
    pub cdr: Option<String>,
    pub disposition: Option<String>,
    pub duration: Option<i64>,
    pub call_duration: Option<i64>,
    pub wait_duration: Option<i64>,
    pub ring_duration: Option<i64>,
    pub answer_by: Option<String>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub ready: Option<DateTime<Utc>>,
    pub cdr_start: Option<DateTime<Utc>>,
    pub cdr_wait: Option<i64>,
    pub group_id: Option<String>,
    pub failed_times: Option<i64>,
    pub init_pos: Option<i64>,
    pub hangup_pos: Option<i64>,
}

impl CacheKey for QueueRecord {
    fn cache_keys(&self) -> Vec<String> {
        vec![
            format!(
                "nebula:cache:queue_record:group:{}:{}:last_call",
                self.group_id.as_deref().unwrap_or(""),
                self.answer_by.as_deref().unwrap_or("")
            ),
            format!(
                "nebula:cache:queue_record:queue:{}:{}:last_call",
                self.queue_id,
                self.answer_by.as_deref().unwrap_or("")
            ),
        ]
    }
}

#[derive(Insertable, Queryable)]
#[table_name = "queue_records"]
pub struct NewQueueRecord {
    pub start: DateTime<Utc>,
    pub queue_id: String,
    pub group_id: String,
    pub disposition: String,
}

#[derive(AsChangeset, Default)]
#[table_name = "queue_records"]
pub struct QueueRecordChange {
    pub duration: Option<i64>,
    pub cdr: Option<String>,
    pub cdr_start: Option<DateTime<Utc>>,
    pub cdr_wait: Option<i64>,
    pub ready: Option<DateTime<Utc>>,
    pub answer: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    pub answer_by: Option<String>,
    pub disposition: Option<String>,
    pub wait_duration: Option<i64>,
    pub ring_duration: Option<i64>,
    pub call_duration: Option<i64>,
    pub failed_times: Option<i64>,
    pub init_pos: Option<i64>,
    pub hangup_pos: Option<i64>,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct QueueFailedUser {
    pub uuid: Uuid,
    pub group_id: String,
    pub user_id: String,
    pub failed_times: i64,
    pub start: DateTime<Utc>,
    pub queue_record_id: String,
}

#[derive(Insertable, Queryable)]
#[table_name = "queue_failed_users"]
pub struct NewQueueFailedUser {
    pub group_id: String,
    pub user_id: String,
    pub failed_times: i64,
    pub start: DateTime<Utc>,
    pub queue_record_id: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct FaxItem {
    pub id: i64,
    pub uuid: String,
    pub deleted: i64,
    pub tenant_id: String,
    pub status: String,
    pub folder: String,
    pub to: String,
    pub callerid: String,
    pub time: SystemTime,
    pub page: i64,
}

#[derive(Insertable, Queryable)]
#[table_name = "faxes"]
pub struct NewFaxItem {
    pub uuid: String,
    pub deleted: i64,
    pub tenant_id: String,
    pub status: String,
    pub folder: String,
    pub to: String,
    pub callerid: String,
    pub time: SystemTime,
    pub page: i64,
}

#[derive(Deserialize)]
pub struct AlertInfoPatterns {
    pub patterns: Vec<String>,
    pub header: String,
}

fn timezone_tz(timezone: Option<&str>) -> Tz {
    match timezone {
        Some(tz) => match tz.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_e) => UTC,
        },
        None => UTC,
    }
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct SipuserAuxLog {
    pub id: i64,
    pub user_id: String,
    pub time: DateTime<Utc>,
    pub code: String,
    pub set_by: String,
}

impl CacheKey for SipuserAuxLog {
    fn cache_keys(&self) -> Vec<String> {
        vec![format!("nebula:cache:user:{}:last_aux_code", self.user_id)]
    }
}

#[derive(Insertable, Queryable)]
#[table_name = "sipuser_aux_log"]
pub struct NewSipuserAuxLog {
    pub user_id: String,
    pub time: DateTime<Utc>,
    pub code: String,
    pub set_by: String,
}

#[derive(Queryable, Deserialize, Serialize, Debug, Clone)]
pub struct SipuserQueueLoginLog {
    pub id: i64,
    pub user_id: String,
    pub time: DateTime<Utc>,
    pub set_by: String,
    pub queue_id: Option<String>,
    pub login: bool,
}

impl CacheKey for SipuserQueueLoginLog {
    fn cache_keys(&self) -> Vec<String> {
        vec![SipuserQueueLoginLog::parse_cache_key(
            &self.user_id,
            self.queue_id.as_deref(),
        )]
    }
}

impl SipuserQueueLoginLog {
    pub fn parse_cache_key(user_uuid: &str, queue_uuid: Option<&str>) -> String {
        if let Some(queue_uuid) = queue_uuid {
            format!(
                "nebula:cache:user:{}:last_individual_queue_login:{}",
                user_uuid, queue_uuid
            )
        } else {
            format!("nebula:cache:user:{}:last_queue_login", user_uuid)
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct AccountQueueStatsResponse {
    pub queue_group: String,
    pub in_queue: usize,
    pub in_queue_longest: u64,
}

#[derive(Insertable, Queryable)]
#[table_name = "sipuser_queue_login_log"]
pub struct NewSipuserQueueLoginLog {
    pub user_id: String,
    pub time: DateTime<Utc>,
    pub login: bool,
    pub queue_id: Option<String>,
    pub set_by: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AuxCodeResponse {
    pub code: String,
    pub set_by: String,
    pub time: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct IndividualQueueLoginResponse {
    pub login: bool,
    pub set_by: String,
    pub time: String,
    pub queue_id: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct QueueLoginResponse {
    pub login: bool,
    pub set_by: String,
    pub time: String,
    pub queue_id: Option<String>,
}

pub fn is_valid_number(phone_number: &PhoneNumber) -> bool {
    let e164 = phone_number
        .format()
        .mode(phonenumber::Mode::E164)
        .to_string();
    // Libphonenumber doesn't support M2M numbers due to the lack of standardization
    // (see https://github.com/google/libphonenumber/blob/master/FAQ.md#what-about-m2m-machine-to-machine-numbers)
    // We add manual support for specific numbering plans here
    if e164.starts_with("+353") && e164.len() == 16 {
        return true;
    }
    if e164.starts_with("+49180") && e164.len() == 18 {
        return true;
    }
    // Sweden - https://pts.se/globalassets/globala-block/nummertillstand/the-swedish-numbering-plan-for-telephony-according-to-itu---2024-01-08.pdf
    if e164.starts_with("+4671") && e164.len() == 16 {
        return true;
    }
    let cc = phone_number.code().value();

    if cc == 44 || cc == 353 {
        let national = phone_number.national().value();
        match national {
            999 => return true,
            101 => return true,
            111 => return true,
            112 => return true,
            105 => return true,
            119 => return true,
            _ => (),
        }
    } else if cc == 61 {
        let national = phone_number.national().value();
        if national == 0 {
            return true;
        }
    }

    phone_number.is_valid()
}
