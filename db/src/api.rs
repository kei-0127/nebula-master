use crate::message::AbandonedQueueRecord;
use crate::message::AbandonedQueueRecordCalledback;
use crate::message::CallContext;
use crate::message::DialogInfo;
use crate::message::DialogState;

use super::models::*;
use super::schema::*;
use anyhow::{anyhow, Error, Result};
use bigdecimal::BigDecimal;
use chrono::DateTime;
use chrono::FixedOffset;
use chrono::Utc;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use diesel::{self};
use futures::pin_mut;
use futures::stream::{self, StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use nebula_redis::REDIS;
use serde::Deserialize;
use serde::{de, Serialize};
use serde_json;
use std::str::FromStr;
use std::{
    fs,
    time::{Duration, SystemTime},
};
use tokio;
use tokio::task::spawn_blocking;
use tokio_postgres;
use toml;
use uuid::Uuid;

pub const ALL_CHANNELS: &str = "nebula:all_channels";
const CACHE_EXPIRE: u64 = 86400;

lazy_static! {
    pub static ref DB: Database = Database::new_nebula().unwrap();
}

#[derive(Deserialize)]
pub struct Config {
    pub db: String,
}

#[derive(Clone)]
pub struct Database {
    pub pool: Pool<ConnectionManager<PgConnection>>,
}

#[derive(Debug, Deserialize)]
struct QueueMemberSimple {
    #[serde(rename = "type")]
    bridge_type: String,
    uuid: String,
}

impl Database {
    pub fn new(database_url: &str) -> Result<Database, Error> {
        let manager = ConnectionManager::<PgConnection>::new_with_timeout(
            database_url,
            std::time::Duration::from_secs(5),
        );
        let pool = Pool::new(manager)?;
        Ok(Database { pool })
    }

    pub fn new_nebula() -> Result<Database, Error> {
        let contents = fs::read_to_string("/etc/nebula/nebula.conf")?;
        let config: Config = toml::from_str(&contents)?;
        Self::new(&config.db)
    }

    pub async fn monitor_cache() -> Result<()> {
        // TODO:
        // Monitor one table on only one server
        let contents = fs::read_to_string("/etc/nebula/nebula.conf")?;
        let config: Config = toml::from_str(&contents)?;
        let tables = vec![
            "sip_users",
            "numbers",
            "sounds",
            "extensions",
            "shortcodes",
            "hunt_groups",
            "hunt_group_members",
            "trunks",
            "trunk_auth_ips",
            "call_flows",
            "diaries",
            "recording",
            "black_list",
            "mobile_apps",
            "speed_dials",
            "queue_records",
            "queue_groups",
            "rooms",
            "room_members",
            "moh_classes",
            "moh_music",
            "providers",
            "login_as_users",
            "sipuser_aux_log",
            "sipuser_queue_login_log",
            "tenant_extensions",
        ];

        for table in tables {
            let database = config.db.clone();
            let table = table.to_string();
            tokio::spawn(async move {
                loop {
                    if let Err(e) = Self::monitor_table(&database, &table).await {
                        eprintln!("monitor table {} error {}", &table, e);
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            });
        }
        Ok(())
    }

    async fn flush_cache(table: &str, value: &[u8]) -> Result<()> {
        match table {
            "sip_users" => Self::flush_model_cache::<User>(value).await?,
            "numbers" => Self::flush_model_cache::<Number>(value).await?,
            "sounds" => Self::flush_model_cache::<Sound>(value).await?,
            "extensions" => Self::flush_model_cache::<Extension>(value).await?,
            "shortcodes" => Self::flush_model_cache::<ShortCode>(value).await?,
            "trunks" => Self::flush_model_cache::<Trunk>(value).await?,
            "trunk_auth_ips" => {
                Self::flush_model_cache::<TrunkAuthIp>(value).await?
            }
            "call_flows" => Self::flush_model_cache::<CallFlow>(value).await?,
            "diaries" => Self::flush_model_cache::<Diary>(value).await?,
            "recording" => Self::flush_model_cache::<Recording>(value).await?,
            "black_list" => Self::flush_model_cache::<BlackList>(value).await?,
            "mobile_apps" => Self::flush_model_cache::<MobileApp>(value).await?,
            "speed_dials" => Self::flush_model_cache::<SpeedDial>(value).await?,
            "queue_records" => Self::flush_model_cache::<QueueRecord>(value).await?,
            "queue_groups" => Self::flush_model_cache::<QueueGroup>(value).await?,
            "rooms" => Self::flush_model_cache::<Room>(value).await?,
            "room_members" => Self::flush_model_cache::<RoomMember>(value).await?,
            "hunt_groups" => Self::flush_model_cache::<HuntGroup>(value).await?,
            "hunt_group_members" => {
                Self::flush_model_cache::<GroupMember>(value).await?
            }
            "moh_classes" => Self::flush_model_cache::<MohClass>(value).await?,
            "moh_music" => Self::flush_model_cache::<MohMusic>(value).await?,
            "providers" => Self::flush_model_cache::<Provider>(value).await?,
            "tenant_extensions" => {
                Self::flush_model_cache::<TenantExtension>(value).await?
            }
            "login_as_users" => {
                Self::flush_model_cache::<LoginAsUser>(value).await?
            }
            "sipuser_aux_log" => {
                Self::flush_model_cache::<SipuserAuxLog>(value).await?
            }
            "sipuser_queue_login_log" => {
                Self::flush_model_cache::<SipuserQueueLoginLog>(value).await?
            }
            _ => Err(anyhow!("table not found"))?,
        }
        Ok(())
    }

    async fn monitor_table(database: &str, table: &str) -> Result<()> {
        let (client, connection) =
            tokio_postgres::connect(database, tokio_postgres::NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {}", e);
            }
        });
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_nanos();
        let params: Vec<String> = vec![now.to_string()];
        let stream = client
            .query_raw(
                format!(
                    "EXPERIMENTAL CHANGEFEED FOR {} with diff, cursor = $1",
                    table
                )
                .as_str(),
                params,
            )
            .await?;
        pin_mut!(stream);
        while let Some(row) = stream.try_next().await? {
            let value: Vec<u8> = row.get("value");
            if let Err(e) = Self::flush_cache(table, &value).await {
                eprintln!(
                    "flush cache on table {} error {} \n{:?}",
                    table,
                    e,
                    String::from_utf8(value)
                );
            }
        }
        Ok(())
    }

    async fn flush_model_cache<T: CacheKey + de::DeserializeOwned>(
        value: &[u8],
    ) -> Result<()> {
        let value = String::from_utf8(value.to_vec())?;
        let value: serde_json::Value = serde_json::from_str(&value)?;

        if let Some(before) = value.get("before") {
            if let Ok(model) = serde_json::from_value::<T>(before.to_owned()) {
                for key in model.cache_keys() {
                    REDIS.del(&key).await?;
                }
            }
        }

        let model: T = serde_json::from_value(
            value
                .get("after")
                .ok_or_else(|| anyhow!("no after in value"))?
                .to_owned(),
        )?;
        for key in model.cache_keys() {
            REDIS.del(&key).await?;
        }
        Ok(())
    }

    pub async fn get_cache<T: de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Option<Option<T>> {
        let result: String = REDIS.get(key).await.ok()?;
        if result == "None" {
            return Some(None);
        }
        let o = serde_json::from_str::<T>(&result).ok()?;
        Some(Some(o))
    }

    pub async fn put_cache<T: Serialize>(&self, key: &str, o: Option<&T>) {
        if let Some(o) = o {
            if let Ok(result) = serde_json::to_string(o) {
                let _ = REDIS.setex(key, CACHE_EXPIRE, &result).await;
            }
        } else {
            let _ = REDIS.setex(key, CACHE_EXPIRE, "None").await;
        }
    }

    pub async fn get_registration_providers_at_location(
        &self,
        location: &str,
    ) -> Option<Vec<Provider>> {
        let key = format!("nebula:cache:registration_provider:{}", location);

        if let Some(providers) = self.get_cache::<Vec<Provider>>(&key).await {
            return providers;
        }

        let pool = self.pool.clone();
        let location = location.to_string();
        let providers = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            providers::table
                .filter(providers::location.eq(location))
                .load::<Provider>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, providers.as_ref()).await;
        providers
    }

    pub async fn get_provider(&self, host: &str) -> Option<Provider> {
        let key = format!("nebula:cache:provider:{}", host);

        if let Some(provider) = self.get_cache::<Provider>(&key).await {
            return provider;
        }

        let pool = self.pool.clone();
        let host = host.to_string();
        let provider = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            providers::table
                .filter(providers::host.eq(host))
                .first::<Provider>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, provider.as_ref()).await;
        provider
    }

    pub async fn get_provider_by_realm(&self, realm: &str) -> Option<Provider> {
        let key = format!("nebula:cache:provider:realm:{}", realm);

        if let Some(provider) = self.get_cache::<Provider>(&key).await {
            return provider;
        }

        let pool = self.pool.clone();
        let realm = realm.to_string();
        let provider = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            providers::table
                .filter(providers::realm.eq(realm))
                .first::<Provider>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, provider.as_ref()).await;
        provider
    }

    pub async fn get_user_by_device_token(
        &self,
        device: &str,
        device_token: &str,
    ) -> Option<User> {
        let app = self.get_mobile_app(device, device_token).await?;
        self.get_user("admin", &app.user).await
    }

    pub async fn check_user_permission(
        &self,
        tenant_id: &str,
        from_user: &str,
        from_groups: Vec<String>,
        to_user: &str,
        to_groups: Vec<String>,
        feature: &str,
    ) -> bool {
        let tenant_id = tenant_id.to_string();
        let from_user = from_user.to_string();
        let to_user = to_user.to_string();
        let feature = feature.to_string();
        let pool = self.pool.clone();
        let permission = spawn_blocking(move || -> Option<SipuserPermission> {
            let db_conn = pool.get().ok()?;
            sipuser_permissions::table
                .filter(
                    sipuser_permissions::from_user
                        .eq(from_user)
                        .or(sipuser_permissions::from_group.eq_any(from_groups)),
                )
                .filter(
                    sipuser_permissions::to_user
                        .eq(to_user)
                        .or(sipuser_permissions::to_group.eq_any(to_groups)),
                )
                .filter(sipuser_permissions::feature.eq(feature))
                .filter(sipuser_permissions::deleted.eq(""))
                .filter(sipuser_permissions::tenant_id.eq(tenant_id))
                .first::<SipuserPermission>(&db_conn)
                .ok()
        })
        .await
        .ok();
        permission.is_some() && permission.unwrap().is_some()
    }

    pub async fn check_sipuser_permission(
        &self,
        from_user: &str,
        to_user: &str,
        feature: &str,
    ) -> bool {
        let pool = self.pool.clone();
        let from_user = from_user.to_string();
        let to_user = to_user.to_string();
        let feature = feature.to_string();
        let permission = spawn_blocking(move || -> Option<SipuserPermission> {
            let db_conn = pool.get().ok()?;
            sipuser_permissions::table
                .filter(sipuser_permissions::from_user.eq(from_user))
                .filter(sipuser_permissions::to_user.eq(to_user))
                .filter(sipuser_permissions::feature.eq(feature))
                .filter(sipuser_permissions::deleted.eq(""))
                .first::<SipuserPermission>(&db_conn)
                .ok()
        })
        .await
        .ok();
        permission.is_some() && permission.unwrap().is_some()
    }

    pub async fn get_user_by_voicemail(
        &self,
        tenant_id: &str,
        voicemail_user_uuid: &str,
    ) -> Option<Vec<User>> {
        let pool = self.pool.clone();
        let voicemail_user_uuid = voicemail_user_uuid.to_string();
        let tenant_id = tenant_id.to_string();
        let users = spawn_blocking(move || -> Option<Vec<User>> {
            let db_conn = pool.get().ok()?;
            sip_users::table
                .filter(sip_users::tenant_id.eq(tenant_id))
                .filter(sip_users::personal_vmbox.eq(voicemail_user_uuid))
                .filter(sip_users::deleted.eq(0))
                .load::<User>(&db_conn)
                .ok()
        })
        .await
        .ok()??;
        Some(users)
    }

    pub async fn delete_login_user(&self, user: &str, login_as: &str) -> Result<()> {
        let pool = self.pool.clone();
        let user = user.to_string();
        let login_as = login_as.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                login_as_users::dsl::login_as_users
                    .filter(login_as_users::user.eq(&user))
                    .filter(login_as_users::login_as.eq(&login_as))
                    .filter(login_as_users::deleted.eq("")),
            )
            .set((
                login_as_users::deleted.eq(diesel::dsl::sql("uuid::STRING")),
                login_as_users::deleted_at.eq(Some(Utc::now())),
            ))
            .get_result::<LoginAsUser>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await??;
        Ok(())
    }

    pub async fn create_login_user(
        &self,
        user: &str,
        login_as: &str,
    ) -> Result<LoginAsUser> {
        let pool = self.pool.clone();
        let user = user.to_string();
        let login_as = login_as.to_string();
        let login_as_user = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let created_at = Utc::now();
            let new_login_as_user = NewLoginAsUser {
                created_at,
                deleted: "".to_string(),
                user,
                login_as,
            };
            diesel::insert_into(login_as_users::table)
                .values(&new_login_as_user)
                .get_result::<LoginAsUser>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(login_as_user)
    }

    // get all the users that is login in as login_as user
    pub async fn clear_login_users(&self, user: &str) -> Result<()> {
        let pool = self.pool.clone();
        let user = user.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                login_as_users::dsl::login_as_users
                    .filter(login_as_users::user.eq(&user))
                    .filter(login_as_users::deleted.eq("")),
            )
            .set((
                login_as_users::deleted.eq(diesel::dsl::sql("uuid::STRING")),
                login_as_users::deleted_at.eq(Some(Utc::now())),
            ))
            .get_result::<LoginAsUser>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await??;
        Ok(())
    }

    // get all the users that is login in as login_as user
    pub async fn get_login_users(&self, login_as: &str) -> Option<Vec<String>> {
        let key = format!("nebula:cache:login_as_user:{login_as}");

        if let Some(users) = self.get_cache::<Vec<String>>(&key).await {
            return users;
        }

        let pool = self.pool.clone();
        let login_as = login_as.to_string();
        let users = spawn_blocking(move || -> Option<Vec<String>> {
            let db_conn = pool.get().ok()?;
            login_as_users::table
                .filter(login_as_users::login_as.eq(&login_as))
                .filter(login_as_users::deleted.eq(""))
                .load::<LoginAsUser>(&db_conn)
                .ok()
                .map(|l| l.into_iter().map(|l| l.user).collect())
        })
        .await
        .ok()?;
        self.put_cache(&key, users.as_ref()).await;
        users
    }

    // get all the users that is login in as login_as user
    pub async fn get_login_from_users(
        &self,
        login_from: &str,
    ) -> Option<Vec<String>> {
        let key = format!("nebula:cache:login_from_user:{login_from}");

        if let Some(users) = self.get_cache::<Vec<String>>(&key).await {
            return users;
        }

        let pool = self.pool.clone();
        let login_from = login_from.to_string();
        let users = spawn_blocking(move || -> Option<Vec<String>> {
            let db_conn = pool.get().ok()?;
            login_as_users::table
                .filter(login_as_users::user.eq(&login_from))
                .filter(login_as_users::deleted.eq(""))
                .load::<LoginAsUser>(&db_conn)
                .ok()
                .map(|l| l.into_iter().map(|l| l.login_as).collect())
        })
        .await
        .ok()?;
        self.put_cache(&key, users.as_ref()).await;
        users
    }

    pub async fn get_msteam_user_in_account(
        &self,
        msteam_domain: &str,
    ) -> Option<User> {
        let pool = self.pool.clone();
        let msteam_domain = msteam_domain.to_string();

        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sip_users::table
                .filter(sip_users::msteam_domain.eq(msteam_domain))
                .filter(sip_users::deleted.eq(0))
                .first::<User>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_user_by_msteam(
        &self,
        msteam: &str,
        msteam_domain: &str,
    ) -> Option<User> {
        let key = format!("nebula:cache:user:msteam:{msteam}:{msteam_domain}");

        if let Some(user) = self.get_cache::<User>(&key).await {
            return user;
        }

        let pool = self.pool.clone();
        let msteam = msteam.to_string();
        let msteam_domain = msteam_domain.to_string();
        let user = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sip_users::table
                .filter(sip_users::msteam.eq(msteam))
                .filter(sip_users::msteam_domain.eq(msteam_domain))
                .filter(sip_users::deleted.eq(0))
                .first::<User>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, user.as_ref()).await;
        user
    }

    pub async fn get_user_by_name(&self, name: &str) -> Option<User> {
        let name = name.to_lowercase();

        let key = format!("nebula:cache:user:name:{}", name);

        if let Some(user) = self.get_cache::<User>(&key).await {
            return user;
        }

        let pool = self.pool.clone();
        let user = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sip_users::table
                .filter(sip_users::name_lower.eq(&name))
                .filter(sip_users::deleted.eq(0))
                .first::<User>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, user.as_ref()).await;
        user
    }

    pub async fn get_extension(
        &self,
        tenant_uuid: &str,
        number: i64,
    ) -> Option<Extension> {
        let key = format!("nebula:cache:extension:{}:{}", tenant_uuid, number);

        if let Some(extension) = self.get_cache::<Extension>(&key).await {
            return extension.filter(|e| e.tenant_id == tenant_uuid);
        }

        let pool = self.pool.clone();
        let local_tenant_uuid = tenant_uuid.to_string();
        let extension = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            extensions::table
                .filter(extensions::number.eq(number))
                .filter(extensions::tenant_id.eq(local_tenant_uuid))
                .filter(extensions::deleted.eq(0))
                .first::<Extension>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, extension.as_ref()).await;
        extension.filter(|e| e.tenant_id == tenant_uuid)
    }

    pub async fn get_in_channel_shortcodes(
        &self,
        tenant_uuid: &str,
    ) -> Option<Vec<ShortCode>> {
        let key = format!("nebula:cache:shortcodes:in_channel:{}", tenant_uuid);

        if let Some(shortcodes) = self.get_cache::<Vec<ShortCode>>(&key).await {
            return shortcodes;
        }

        let pool = self.pool.clone();
        let tenant_uuid = tenant_uuid.to_string();
        let shortcodes = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            shortcodes::table
                .filter(shortcodes::tenant_id.eq(tenant_uuid))
                .filter(shortcodes::deleted.eq(0))
                .filter(shortcodes::in_channel.eq(true))
                .load::<ShortCode>(&db_conn)
                .ok()
                .as_mut()
                .map(|shortcodes| {
                    shortcodes
                        .sort_by(|a, b| a.shortcode.len().cmp(&b.shortcode.len()));
                    shortcodes.to_owned()
                })
        })
        .await
        .ok()?;

        self.put_cache(&key, shortcodes.as_ref()).await;
        shortcodes
    }

    pub async fn get_shortcodes(&self, tenant_uuid: &str) -> Option<Vec<ShortCode>> {
        let key = format!("nebula:cache:shortcodes:{}", tenant_uuid);

        if let Some(shortcodes) = self.get_cache::<Vec<ShortCode>>(&key).await {
            return shortcodes;
        }

        let pool = self.pool.clone();
        let tenant_uuid = tenant_uuid.to_string();
        let shortcodes = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            shortcodes::table
                .filter(shortcodes::tenant_id.eq(tenant_uuid))
                .filter(shortcodes::deleted.eq(0))
                .load::<ShortCode>(&db_conn)
                .ok()
                .as_mut()
                .map(|shortcodes| {
                    shortcodes.sort_by_key(|s| -(s.shortcode.len() as i32));
                    shortcodes.to_owned()
                })
        })
        .await
        .ok()?;
        self.put_cache(&key, shortcodes.as_ref()).await;
        shortcodes
    }

    pub async fn get_tenant_extensions(
        &self,
        tenant_uuid: &str,
    ) -> Option<Vec<TenantExtension>> {
        let key = format!("nebula:cache:tenant_extensions:{tenant_uuid}");

        if let Some(tenant_extensions) =
            self.get_cache::<Vec<TenantExtension>>(&key).await
        {
            return tenant_extensions;
        }

        let pool = self.pool.clone();
        let tenant_uuid = tenant_uuid.to_string();
        let tenant_extensions = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            tenant_extensions::table
                .filter(tenant_extensions::tenant_id.eq(tenant_uuid))
                .filter(tenant_extensions::deleted.eq(""))
                .load::<TenantExtension>(&db_conn)
                .ok()
                .as_mut()
                .map(|tenant_extensions| {
                    tenant_extensions
                        .sort_by_key(|s| -(s.number.to_string().len() as i32));
                    tenant_extensions.to_owned()
                })
        })
        .await
        .ok()?;
        self.put_cache(&key, tenant_extensions.as_ref()).await;
        tenant_extensions
    }

    pub async fn get_no_star_shortcodes(
        &self,
        tenant_uuid: &str,
    ) -> Option<Vec<ShortCode>> {
        let key = format!("nebula:cache:shortcodes:no_star:{}", tenant_uuid);

        if let Some(shortcodes) = self.get_cache::<Vec<ShortCode>>(&key).await {
            return shortcodes;
        }

        let pool = self.pool.clone();
        let tenant_uuid = tenant_uuid.to_string();
        let shortcodes = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            shortcodes::table
                .filter(shortcodes::tenant_id.eq(tenant_uuid))
                .filter(shortcodes::deleted.eq(0))
                .filter(shortcodes::no_star.eq(true))
                .load::<ShortCode>(&db_conn)
                .ok()
                .as_mut()
                .map(|shortcodes| {
                    shortcodes.sort_by_key(|s| -(s.shortcode.len() as i32));
                    shortcodes.to_owned()
                })
        })
        .await
        .ok()?;
        self.put_cache(&key, shortcodes.as_ref()).await;
        shortcodes
    }

    pub async fn get_speed_dial(
        &self,
        tenant_id: &str,
        code: &str,
    ) -> Option<SpeedDial> {
        let key = format!("nebula:cache:speed_dial:{}:{}", tenant_id, code);

        if let Some(speed_dial) = self.get_cache::<SpeedDial>(&key).await {
            return speed_dial;
        }

        let tenant_id = tenant_id.to_string();
        let code = code.to_string();
        let pool = self.pool.clone();
        let speed_dial = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            speed_dials::table
                .filter(speed_dials::tenant_id.eq(tenant_id))
                .filter(speed_dials::deleted.eq(""))
                .filter(speed_dials::code.eq(code))
                .first::<SpeedDial>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, speed_dial.as_ref()).await;
        speed_dial
    }

    pub async fn get_room(&self, room: &str) -> Option<Room> {
        let key = format!("nebula:cache:room:{}", room);

        if let Some(room) = self.get_cache::<Room>(&key).await {
            return room;
        }

        let room = room.to_string();
        let pool = self.pool.clone();
        let room = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            rooms::table
                .filter(rooms::deleted.eq(""))
                .filter(rooms::uuid.eq(Uuid::from_str(&room).ok()?))
                .first::<Room>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, room.as_ref()).await;
        room
    }

    pub async fn get_room_member(
        &self,
        room: &str,
        user: &str,
    ) -> Option<RoomMember> {
        let key = format!("nebula:cache:room:member:{}:{}", room, user);

        if let Some(members) = self.get_cache::<RoomMember>(&key).await {
            return members;
        }

        let room = room.to_string();
        let user = user.to_string();
        let pool = self.pool.clone();
        let member = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            room_members::table
                .filter(room_members::room_id.eq(room))
                .filter(room_members::user_id.eq(user))
                .filter(room_members::deleted.eq(""))
                .first::<RoomMember>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, member.as_ref()).await;
        member
    }

    pub async fn get_room_members(&self, room: &str) -> Option<Vec<RoomMember>> {
        let key = format!("nebula:cache:room:members:{}", room);

        if let Some(members) = self.get_cache::<Vec<RoomMember>>(&key).await {
            return members;
        }

        let room = room.to_string();
        let pool = self.pool.clone();
        let members = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            room_members::table
                .filter(room_members::room_id.eq(room))
                .filter(room_members::deleted.eq(""))
                .load::<RoomMember>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, members.as_ref()).await;
        members
    }

    pub async fn get_shortcode(
        &self,
        tenant_uuid: &str,
        code: &str,
    ) -> Option<ShortCode> {
        let key = format!("nebula:cache:shortcode:{}:{}", tenant_uuid, code);

        if let Some(shortcode) = self.get_cache::<ShortCode>(&key).await {
            return shortcode;
        }

        let tenant_uuid = tenant_uuid.to_string();
        let code = code.to_string();
        let pool = self.pool.clone();
        let shortcode = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            shortcodes::table
                .filter(shortcodes::tenant_id.eq(tenant_uuid))
                .filter(shortcodes::deleted.eq(0))
                .filter(shortcodes::shortcode.eq(code))
                .first::<ShortCode>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, shortcode.as_ref()).await;
        shortcode
    }

    pub async fn get_user_last_queue_call_by_group(
        &self,
        user: &str,
        group_id: &str,
    ) -> Option<QueueRecord> {
        let key = format!(
            "nebula:cache:queue_record:group:{}:{}:last_call",
            group_id, user,
        );

        if let Some(record) = self.get_cache::<QueueRecord>(&key).await {
            return record;
        }

        let user = user.to_string();
        let group_id = group_id.to_string();
        let pool = self.pool.clone();
        let record = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_records::table
                .filter(queue_records::answer_by.eq(user))
                .filter(queue_records::group_id.eq(group_id))
                .order_by(queue_records::start.desc())
                .first::<QueueRecord>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, record.as_ref()).await;
        record
    }

    pub async fn get_user_last_queue_call_by_queue(
        &self,
        user: &str,
        queue_id: &str,
    ) -> Option<QueueRecord> {
        let key = format!(
            "nebula:cache:queue_record:queue:{}:{}:last_call",
            queue_id, user,
        );

        if let Some(record) = self.get_cache::<QueueRecord>(&key).await {
            return record;
        }

        let user = user.to_string();
        let queue_id = queue_id.to_string();
        let pool = self.pool.clone();
        let record = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_records::table
                .filter(queue_records::answer_by.eq(user))
                .filter(queue_records::queue_id.eq(queue_id))
                .order_by(queue_records::start.desc())
                .first::<QueueRecord>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, record.as_ref()).await;
        record
    }

    pub async fn get_user(
        &self,
        tenant_uuid: &str,
        user_uuid: &str,
    ) -> Option<User> {
        let key = format!("nebula:cache:user:uuid:{}", user_uuid);

        if let Some(user) = self.get_cache::<User>(&key).await {
            return user
                .filter(|u| tenant_uuid == "admin" || u.tenant_id == tenant_uuid);
        }

        let pool = self.pool.clone();
        let user_uuid = user_uuid.to_string();
        let user = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sip_users::table
                .filter(sip_users::uuid.eq(user_uuid))
                .filter(sip_users::deleted.eq(0))
                .first::<User>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, user.as_ref()).await;
        user.filter(|u| tenant_uuid == "admin" || u.tenant_id == tenant_uuid)
    }

    pub async fn user_last_queue_login(
        &self,
        user_id: &str,
    ) -> Option<SipuserQueueLoginLog> {
        let key = SipuserQueueLoginLog::parse_cache_key(user_id, None);
        if let Some(cache) = self.get_cache::<SipuserQueueLoginLog>(&key).await {
            return cache;
        }

        let user_id = user_id.to_string();
        let pool = self.pool.clone();
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

        self.put_cache(&key, login.as_ref()).await;
        login
    }

    pub async fn user_last_individual_queue_login(
        &self,
        user_id: &str,
        queue_id: &str,
    ) -> Option<SipuserQueueLoginLog> {
        let key = SipuserQueueLoginLog::parse_cache_key(&user_id, Some(queue_id));
        if let Some(cache) = self.get_cache::<SipuserQueueLoginLog>(&key).await {
            return cache;
        }

        let user_id = user_id.to_string();
        let queue_id = queue_id.to_string();
        let pool = self.pool.clone();
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

        self.put_cache(&key, login.as_ref()).await;
        login
    }

    pub async fn update_user_available(&self, uuid: String, available: bool) {
        let pool = self.pool.clone();
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                sip_users::dsl::sip_users.filter(sip_users::uuid.eq(uuid)),
            )
            .set(sip_users::available.eq(available))
            .get_result::<User>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn update_user_global_forward(
        &self,
        uuid: String,
        global_forward: Option<String>,
    ) {
        let pool = self.pool.clone();
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                sip_users::dsl::sip_users.filter(sip_users::uuid.eq(uuid)),
            )
            .set(sip_users::global_forward.eq(global_forward))
            .get_result::<User>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn update_number(
        &self,
        id: i64,
        change: NumberChange,
    ) -> Result<Number> {
        let pool = self.pool.clone();
        let number = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(numbers::dsl::numbers.find(&id))
                .set(&change)
                .get_result::<Number>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;
        Ok(number)
    }

    pub async fn get_black_list(&self, tenant_id: &str) -> Option<BlackList> {
        let key = format!("nebula:cache:black_list:{}", tenant_id);

        if let Some(blacklist) = self.get_cache::<BlackList>(&key).await {
            return blacklist;
        }

        let pool = self.pool.clone();
        let tenant_id = tenant_id.to_string();
        let blacklist = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            black_list::table
                .filter(black_list::tenant_id.eq(tenant_id))
                .filter(black_list::deleted.eq(0))
                .first::<BlackList>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, blacklist.as_ref()).await;
        blacklist
    }

    pub async fn get_number(&self, tenant_id: &str, e164: &str) -> Option<Number> {
        let key = format!("nebula:cache:number:{}", e164);

        if let Some(number) = self.get_cache::<Number>(&key).await {
            return number.filter(|n| {
                tenant_id == "admin"
                    || n.tenant_id.clone().unwrap_or("".to_string()) == tenant_id
            });
        }

        let e164 = e164.to_string();
        let pool = self.pool.clone();
        let number = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            numbers::table
                .filter(numbers::e164.eq(e164))
                .filter(numbers::deleted.eq(0))
                .first::<Number>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        self.put_cache(&key, number.as_ref()).await;
        number.filter(|n| {
            tenant_id == "admin"
                || n.tenant_id.clone().unwrap_or("".to_string()) == tenant_id
        })
    }

    pub async fn get_number_by_uuid(&self, uuid: &str) -> Option<Number> {
        let key = format!("nebula:cache:number:uuid:{}", uuid);

        if let Some(number) = self.get_cache::<Number>(&key).await {
            return number;
        }

        let pool = self.pool.clone();
        let uuid = uuid.to_string();
        let number = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            numbers::table
                .filter(numbers::uuid.eq(uuid))
                .filter(numbers::deleted.eq(0))
                .first::<Number>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, number.as_ref()).await;
        number
    }

    pub async fn get_sound(&self, uuid: &str) -> Option<Sound> {
        let key = format!("nebula:cache:sound:{}", uuid);

        if let Some(sound) = self.get_cache::<Sound>(&key).await {
            return sound;
        }

        let pool = self.pool.clone();

        let uuid = uuid.to_string();
        let sound = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            sounds::table
                .filter(sounds::uuid.eq(uuid))
                .filter(sounds::deleted.eq(0))
                .first::<Sound>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, sound.as_ref()).await;
        sound
    }

    pub async fn get_moh_class(&self, moh_class: &str) -> Option<MohClass> {
        let key = format!("nebula:cache:moh_class:{moh_class}");

        if let Some(moh) = self.get_cache::<MohClass>(&key).await {
            return moh;
        }

        let pool = self.pool.clone();
        let local_moh_class = moh_class.to_string();
        let moh = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            moh_classes::table
                .filter(moh_classes::uuid.eq(local_moh_class))
                .filter(moh_classes::deleted.eq(0))
                .first::<MohClass>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, moh.as_ref()).await;
        moh
    }

    pub async fn get_moh_musics(&self, moh_class: &str) -> Option<Vec<MohMusic>> {
        let key = format!("nebula:cache:moh_music:{moh_class}");

        if let Some(musics) = self.get_cache::<Vec<MohMusic>>(&key).await {
            return musics;
        }

        let pool = self.pool.clone();
        let local_moh_class = moh_class.to_string();
        let musics = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            moh_music::table
                .filter(moh_music::moh_class_uuid.eq(local_moh_class))
                .filter(moh_music::deleted.eq(0))
                .load::<MohMusic>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, musics.as_ref()).await;
        musics
    }

    pub async fn get_music_on_hold(&self, moh_class: &str) -> Option<MusicOnHold> {
        let moh = self.get_moh_class(moh_class).await?;

        let musics = self.get_moh_musics(moh_class).await?;

        let sounds = musics
            .iter()
            .map(|m| m.sound_uuid.clone())
            .collect::<Vec<String>>();
        Some(MusicOnHold {
            play_id: moh.uuid,
            sounds,
            random: moh.random,
        })
    }

    pub async fn save_voicemail_msg(&self, msg: &VoicemailMessage) {
        let pool = self.pool.clone();
        let msg_uuid = msg.uuid.clone();
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                voicemail_messages::dsl::voicemail_messages
                    .filter(voicemail_messages::uuid.eq(&msg_uuid)),
            )
            .set(voicemail_messages::folder.eq("saved"))
            .get_result::<VoicemailMessage>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn update_voicemail_password(
        &self,
        vm_user: &VoicemailUser,
        password: &str,
    ) {
        let pool = self.pool.clone();
        let user_id = vm_user.id;
        let password = password.to_string();
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                voicemail_users::dsl::voicemail_users
                    .filter(voicemail_users::id.eq(user_id)),
            )
            .set(voicemail_users::password.eq(password))
            .get_result::<VoicemailUser>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn update_voicemail_greeting(&self, user_id: i64, greeting: &str) {
        let pool = self.pool.clone();
        let greeting = greeting.to_string();
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                voicemail_users::dsl::voicemail_users
                    .filter(voicemail_users::id.eq(user_id)),
            )
            .set(voicemail_users::greeting.eq(greeting))
            .get_result::<VoicemailUser>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn delete_voicemail_msg(&self, msg: &VoicemailMessage) {
        let pool = self.pool.clone();
        let msg_uuid = msg.uuid.clone();
        let msg_id = msg.id;
        let _ = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(
                voicemail_messages::dsl::voicemail_messages
                    .filter(voicemail_messages::uuid.eq(&msg_uuid)),
            )
            .set((
                voicemail_messages::deleted.eq(msg_id),
                voicemail_messages::deleted_at.eq(Some(SystemTime::now())),
            ))
            .get_result::<VoicemailMessage>(&db_conn)
            .map_err(|e| anyhow!(e))
        })
        .await;
    }

    pub async fn get_voicemail_user(
        &self,
        tenant_id: &str,
        uuid: &str,
    ) -> Option<VoicemailUser> {
        let pool = self.pool.clone();
        let uuid = uuid.to_string();
        let voicemail_user = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            voicemail_users::table
                .filter(voicemail_users::uuid.eq(uuid))
                .filter(voicemail_users::deleted.eq(0))
                .first::<VoicemailUser>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        voicemail_user.filter(|u| tenant_id == "admin" || u.tenant_id == tenant_id)
    }

    pub async fn get_user_voicemail(
        &self,
        user_uuid: &str,
    ) -> Option<VoicemailUser> {
        let pool = self.pool.clone();
        let user_uuid = user_uuid.to_string();
        let voicemail_member = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            voicemail_members::table
                .filter(voicemail_members::deleted.eq(0))
                .filter(voicemail_members::member_uuid.eq(user_uuid))
                .filter(voicemail_members::discriminator.eq("sipuser"))
                .first::<VoicemailMember>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        match voicemail_member {
            Some(member) => {
                self.get_voicemail_user("admin", &member.voicemail_user_uuid)
                    .await
            }
            None => None,
        }
    }

    pub async fn get_mobile_app(
        &self,
        device: &str,
        device_token: &str,
    ) -> Option<MobileApp> {
        let key = format!("nebula:cache:mobile_app:{device}:{device_token}");

        if let Some(app) = self.get_cache::<MobileApp>(&key).await {
            return app;
        }

        let pool = self.pool.clone();
        let device = device.to_string();
        let device_token = device_token.to_string();
        let app = spawn_blocking(move || -> Option<MobileApp> {
            let db_conn = pool.get().ok()?;
            mobile_apps::table
                .filter(mobile_apps::device.eq(device))
                .filter(mobile_apps::device_token.eq(device_token))
                .filter(mobile_apps::deleted.eq(""))
                .first::<MobileApp>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        self.put_cache(&key, app.as_ref()).await;
        app
    }

    pub async fn get_mobile_apps(&self, user: &User) -> Option<Vec<MobileApp>> {
        let key = format!("nebula:cache:user:uuid:{}:mobile_apps", user.uuid);

        if let Some(apps) = self.get_cache::<Vec<MobileApp>>(&key).await {
            return apps;
        }

        let pool = self.pool.clone();
        let user_uuid = user.uuid.clone();
        let mobile_apps = spawn_blocking(move || -> Option<Vec<MobileApp>> {
            let db_conn = pool.get().ok()?;
            mobile_apps::table
                .filter(mobile_apps::user.eq(&user_uuid))
                .filter(mobile_apps::deleted.eq(""))
                .order_by(mobile_apps::user.desc())
                .order_by(mobile_apps::deleted.desc())
                .order_by(mobile_apps::created_at.desc())
                .limit(5)
                .load::<MobileApp>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        self.put_cache(&key, mobile_apps.as_ref()).await;
        mobile_apps
    }

    pub async fn get_trunk(&self, tenant_id: &str, uuid: &str) -> Option<Trunk> {
        let key = format!("nebula:cache:trunk:{}", uuid);

        if let Some(trunk) = self.get_cache::<Trunk>(&key).await {
            return trunk
                .filter(|t| tenant_id == "admin" || t.tenant_id == tenant_id);
        }

        let pool = self.pool.clone();
        let uuid = uuid.to_string();
        let trunk = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            trunks::table
                .filter(trunks::uuid.eq(uuid))
                .filter(trunks::deleted.eq(0))
                .first::<Trunk>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, trunk.as_ref()).await;
        trunk.filter(|t| tenant_id == "admin" || t.tenant_id == tenant_id)
    }

    pub async fn get_trunk_by_ip(&self, ip: &str, info: &str) -> Option<TrunkAuth> {
        let key = format!("nebula:cache:trunk:ip:{ip}:{info}");

        if let Some(trunk_auth) = self.get_cache::<TrunkAuth>(&key).await {
            return trunk_auth;
        }

        let pool = self.pool.clone();
        let ip = ip.to_string();
        let info = info.to_string();
        let trunk_auth_ip = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            trunk_auth_ips::table
                .filter(trunk_auth_ips::deleted.eq(0))
                .filter(trunk_auth_ips::ip.eq(ip))
                .filter(trunk_auth_ips::info.eq(info))
                .first::<TrunkAuthIp>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        let trunk_auth = match trunk_auth_ip {
            Some(a) => {
                let pool = self.pool.clone();
                spawn_blocking(move || {
                    let db_conn = pool.get().ok()?;
                    trunk_auths::table
                        .filter(trunk_auths::deleted.eq(0))
                        .filter(trunk_auths::discriminator.eq("trunkauthip"))
                        .filter(trunk_auths::member_uuid.eq(a.uuid))
                        .first::<TrunkAuth>(&db_conn)
                        .ok()
                })
                .await
                .ok()?
            }
            None => None,
        };

        self.put_cache(&key, trunk_auth.as_ref()).await;
        trunk_auth
    }

    pub async fn get_trunk_by_user(&self, user: &str) -> Option<TrunkAuth> {
        let key = format!("nebula:cache:trunk:user:{}", user);

        if let Some(trunk_auth) = self.get_cache::<TrunkAuth>(&key).await {
            return trunk_auth;
        }

        let pool = self.pool.clone();
        let user = user.to_string();
        let trunk_auth = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            trunk_auths::table
                .filter(trunk_auths::deleted.eq(0))
                .filter(trunk_auths::discriminator.eq("sipuser"))
                .filter(trunk_auths::member_uuid.eq(user))
                .first::<TrunkAuth>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        self.put_cache(&key, trunk_auth.as_ref()).await;
        trunk_auth
    }

    pub async fn get_callflow(
        &self,
        tenant_id: &str,
        uuid: &str,
    ) -> Option<CallFlow> {
        let key = format!("nebula:cache:callflow:{}", uuid);

        if let Some(callflow) = self.get_cache::<CallFlow>(&key).await {
            return callflow
                .filter(|f| tenant_id == "admin" || f.tenant_id == tenant_id);
        }

        let pool = self.pool.clone();
        let uuid = uuid.to_string();
        let callflow = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            call_flows::table
                .filter(call_flows::uuid.eq(uuid))
                .filter(call_flows::deleted.eq(0))
                .first::<CallFlow>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, callflow.as_ref()).await;
        callflow.filter(|f| tenant_id == "admin" || f.tenant_id == tenant_id)
    }

    pub async fn get_diary(&self, tenant_id: &str, uuid: &str) -> Option<Diary> {
        let key = format!("nebula:cache:diary:{}", uuid);

        if let Some(diary) = self.get_cache::<Diary>(&key).await {
            return diary
                .filter(|d| tenant_id == "admin" || d.tenant_id == tenant_id);
        }

        let pool = self.pool.clone();
        let uuid = uuid.to_string();
        let diary = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            diaries::table
                .filter(diaries::uuid.eq(Uuid::from_str(&uuid).ok()?))
                .filter(diaries::deleted.eq(""))
                .first::<Diary>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, diary.as_ref()).await;
        diary.filter(|d| tenant_id == "admin" || d.tenant_id == tenant_id)
    }

    pub async fn get_recording(&self, tenant_id: &str) -> Option<Recording> {
        let key = format!("nebula:cache:recording:{}", tenant_id);

        if let Some(recording) = self.get_cache::<Recording>(&key).await {
            return recording;
        }

        let pool = self.pool.clone();
        let tenant_id = tenant_id.to_string();
        let recording = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            recording::table
                .filter(recording::deleted.eq(0))
                .filter(recording::tenant_id.eq(tenant_id))
                .first::<Recording>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        self.put_cache(&key, recording.as_ref()).await;
        recording
    }

    pub async fn get_queue_hint(&self, hint_uuid: &str) -> Option<QueueHint> {
        let pool = self.pool.clone();
        let hint_uuid = hint_uuid.to_string();
        let hint = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_hints::table
                .filter(queue_hints::uuid.eq(hint_uuid))
                .filter(queue_hints::deleted.eq(0))
                .first::<QueueHint>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        hint
    }

    pub async fn get_queue_records_by_cdr(
        &self,
        cdr_id: &str,
    ) -> Option<Vec<QueueRecord>> {
        let pool = self.pool.clone();
        let cdr_id = cdr_id.to_string();
        let records = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_records::table
                .filter(queue_records::cdr.eq(cdr_id))
                .load::<QueueRecord>(&db_conn)
                .ok()
        })
        .await
        .ok()?;
        records
    }

    /// get the query for abandoned queue records
    /// this is needed because boxed diesel query can't be cloned
    fn abandoned_queue_records_query(
        group_id: String,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
    ) -> diesel::query_builder::BoxedSelectStatement<
        'static,
        queue_records::SqlType,
        queue_records::table,
        diesel::pg::Pg,
    > {
        let mut query = queue_records::table
            .filter(queue_records::group_id.eq(group_id))
            .filter(queue_records::start.ge(start))
            .into_boxed();
        if let Some(end) = end {
            query = query.filter(queue_records::start.le(end));
        }
        query = query.filter(queue_records::disposition.eq("abandoned"));
        query
    }

    pub async fn get_abandoned_queue_records(
        &self,
        group_id: String,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
        limit: i64,
        offset: i64,
    ) -> Option<(i64, Vec<QueueRecord>)> {
        let pool = self.pool.clone();
        let records = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;

            let count: i64 =
                Self::abandoned_queue_records_query(group_id.clone(), start, end)
                    .count()
                    .get_result(&db_conn)
                    .ok()?;

            let records = Self::abandoned_queue_records_query(group_id, start, end)
                .order_by(queue_records::group_id.asc())
                .order_by(queue_records::start.asc())
                .limit(limit)
                .offset(offset)
                .load::<QueueRecord>(&db_conn)
                .ok()?;

            Some((count, records))
        })
        .await
        .ok()?;
        records
    }

    pub async fn abandoned_queue_record_called_back(
        &self,
        queue_record: &QueueRecord,
        needs_to_be_answered: bool,
        timeout: i32,
    ) -> Result<AbandonedQueueRecord> {
        let key = format!(
            "nebula:cache:queue_record_called_back:{}",
            queue_record.uuid
        );

        let cached_record = if let Some(Some(record)) =
            self.get_cache::<AbandonedQueueRecord>(&key).await
        {
            if let AbandonedQueueRecordCalledback::WithinTimeout(start_time) =
                record.called_back
            {
                // we haven't found a called back but the last check was within timeout,
                // so get the necessary cached information to do another query
                Some((
                    record.tenant_id,
                    record.number,
                    record.number_formats,
                    start_time,
                ))
            } else {
                // the called back information is confirmed, so we can just return
                return Ok(record);
            }
        } else {
            // we haven't done any query for this record yet
            None
        };

        let queue_record_time = queue_record.end.unwrap_or(queue_record.start);
        let timeout_time = queue_record_time
            .checked_add_signed(chrono::Duration::seconds(timeout.into()))
            .unwrap_or(queue_record_time);
        let (tenant_id, number, number_formats, start_time) =
            if let Some(cached_record) = cached_record {
                cached_record
            } else {
                // we check the cdr for the queue record to get the called back number
                let cdr = queue_record
                    .cdr
                    .as_deref()
                    .ok_or_else(|| anyhow!("no cdr in queue record"))?;
                let cdr = Uuid::from_str(cdr)?;
                let cdr = self
                    .get_cdr(cdr)
                    .await
                    .ok_or_else(|| anyhow!("can't find cdr"))?;
                let from_exten = cdr
                    .from_exten
                    .ok_or_else(|| anyhow!("cdr doesn't have from exten"))?;
                let number = get_phone_number(&from_exten);
                let number_formats = number
                    .as_ref()
                    .map(|p| {
                        vec![
                            format_national_number(p),
                            p.format().mode(phonenumber::Mode::E164).to_string(),
                            p.format().mode(phonenumber::Mode::E164).to_string()
                                [1..]
                                .to_string(),
                            from_exten.clone(),
                        ]
                    })
                    .unwrap_or_else(|| vec![from_exten.clone()]);
                let number = number
                    .map(|p| p.format().mode(phonenumber::Mode::E164).to_string())
                    .unwrap_or_else(|| from_exten.clone());
                (cdr.tenant_id, number, number_formats, queue_record_time)
            };

        let pool = self.pool.clone();
        let local_number_formats = number_formats.clone();
        let local_tenant_id = tenant_id.clone();
        let (called_back_cdr, next_query_start_time) =
            spawn_blocking(move || -> (Option<CdrItem>, DateTime<Utc>) {
                let db_conn = match pool.get().ok() {
                    Some(conn) => conn,
                    None => return (None, start_time),
                };
                let mut query = cdr_items::table
                    .filter(cdr_items::tenant_id.eq(&local_tenant_id))
                    .filter(cdr_items::start.ge(start_time))
                    .filter(cdr_items::start.le(timeout_time))
                    .filter(cdr_items::call_type.eq("outbound"))
                    .filter(cdr_items::to_exten.eq_any(local_number_formats))
                    .into_boxed();

                if needs_to_be_answered {
                    query = query.filter(cdr_items::disposition.eq("answered"));
                }
                let called_back_cdr = query
                    .order_by(cdr_items::tenant_id.asc())
                    .order_by(cdr_items::start.asc())
                    .first::<CdrItem>(&db_conn)
                    .ok();

                let next_query_start_time = if called_back_cdr.is_none() {
                    // if we haven't found any called back cdr
                    // we need to find the timestamp we cache for the
                    // next query to be more efficient. And to do that,
                    // we need to check the earliest unfinished call if there were any.
                    // If there's no unfinished call, we cache the current time
                    let earlist_unfinished_call = cdr_items::table
                        .filter(cdr_items::tenant_id.eq(local_tenant_id))
                        .filter(cdr_items::start.ge(start_time))
                        .filter(cdr_items::start.le(timeout_time))
                        .filter(cdr_items::end.is_null())
                        .order_by(cdr_items::tenant_id.asc())
                        .order_by(cdr_items::start.asc())
                        .first::<CdrItem>(&db_conn)
                        .ok();
                    earlist_unfinished_call
                        .map(|c| c.start)
                        .unwrap_or_else(Utc::now)
                } else {
                    Utc::now()
                };
                // sub 10 seconds just in case
                let next_query_start_time = next_query_start_time
                    .checked_sub_signed(chrono::Duration::seconds(10))
                    .unwrap_or(next_query_start_time);

                (called_back_cdr, next_query_start_time)
            })
            .await?;

        let called_back = if let Some(called_back_cdr) = called_back_cdr {
            AbandonedQueueRecordCalledback::Calledback(
                called_back_cdr.uuid,
                called_back_cdr.start,
            )
        } else if next_query_start_time > timeout_time {
            AbandonedQueueRecordCalledback::NotCalledback
        } else {
            AbandonedQueueRecordCalledback::WithinTimeout(next_query_start_time)
        };
        let record = AbandonedQueueRecord {
            number,
            number_formats,
            tenant_id,
            called_back,
        };

        self.put_cache(&key, Some(&record)).await;
        Ok(record)
    }

    pub async fn get_cdr_events(&self, cdr_id: Uuid) -> Option<Vec<CdrEvent>> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            cdr_events::table
                .filter(cdr_events::cdr_id.eq(cdr_id))
                .load::<CdrEvent>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_user_last_cdr_event(
        &self,
        user_id: String,
    ) -> Option<CdrEvent> {
        let pool = self.pool.clone();

        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            cdr_events::table
                .filter(cdr_events::user_id.eq(user_id))
                .order_by(cdr_events::start.desc())
                .first::<CdrEvent>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_account_queue_groups(
        &self,
        tenant_id: &str,
    ) -> Option<Vec<QueueGroup>> {
        let key = format!("nebula:cache:account_queue_groups:{tenant_id}");

        if let Some(groups) = self.get_cache::<Vec<QueueGroup>>(&key).await {
            return groups;
        }

        let pool = self.pool.clone();
        let tenant_id = tenant_id.to_string();
        let groups = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_groups::table
                .filter(queue_groups::tenant_id.eq(tenant_id))
                .filter(queue_groups::deleted.eq(0))
                .load::<QueueGroup>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, groups.as_ref()).await;
        groups
    }

    pub async fn get_queue_group(&self, group_uuid: &str) -> Option<QueueGroup> {
        let key = format!("nebula:cache:queue_group:{}", group_uuid);

        if let Some(group) = self.get_cache::<QueueGroup>(&key).await {
            return group;
        }

        let pool = self.pool.clone();
        let group_uuid = group_uuid.to_string();
        let group = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            queue_groups::table
                .filter(queue_groups::uuid.eq(group_uuid))
                .filter(queue_groups::deleted.eq(0))
                .first::<QueueGroup>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, group.as_ref()).await;

        group
    }

    pub async fn get_queue_group_members(
        &self,
        tenant_uuid: &str,
        group_uuid: &str,
    ) -> Option<Vec<String>> {
        let group = self.get_queue_group(group_uuid).await?;

        let Ok(members) =
            serde_json::from_str::<Vec<QueueMemberSimple>>(&group.members)
        else {
            return None;
        };

        let mut users = vec![];

        for member in members {
            match member.bridge_type.as_str() {
                "sipuser" => {
                    users.push(member.uuid);
                }
                "huntgroup" => {
                    if let Some(hg_members) =
                        self.get_group_members(tenant_uuid, &member.uuid).await
                    {
                        for hg_member in hg_members {
                            users.push(hg_member.sip_user_uuid);
                        }
                    }
                }
                _ => {}
            };
        }

        Some(users)
    }

    pub async fn get_voicemail_menu(
        &self,
        tenant_id: &str,
        voicemail_menu_id: &str,
    ) -> Option<VoicemailMenu> {
        let pool = self.pool.clone();

        let voicemail_menu_id = voicemail_menu_id.to_string();
        let voicemail_menu = spawn_blocking(move || -> Option<VoicemailMenu> {
            let db_conn = pool.get().ok()?;
            let menu = voicemail_menus::table
                .filter(voicemail_menus::uuid.eq(voicemail_menu_id))
                .filter(voicemail_menus::deleted.eq(0))
                .first::<VoicemailMenu>(&db_conn)
                .ok();
            menu
        })
        .await
        .ok()?;

        voicemail_menu.filter(|m| tenant_id == "admin" || m.tenant_id == tenant_id)
    }

    pub async fn get_hunt_group(&self, group_uuid: &str) -> Option<HuntGroup> {
        let key = format!("nebula:cache:hunt_group:{}", group_uuid);

        if let Some(group) = self.get_cache::<HuntGroup>(&key).await {
            return group;
        }

        let pool = self.pool.clone();
        let group_uuid = group_uuid.to_string();
        let group = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            hunt_groups::table
                .filter(hunt_groups::uuid.eq(group_uuid))
                .filter(hunt_groups::deleted.eq(0))
                .first::<HuntGroup>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, group.as_ref()).await;

        group
    }

    pub async fn get_user_groups(&self, user_uuid: &str) -> Option<Vec<String>> {
        let key = format!("nebula:cache:user:groups:{user_uuid}");

        if let Some(user) = self.get_cache::<Vec<String>>(&key).await {
            return user;
        }

        let pool = self.pool.clone();
        let user_uuid = user_uuid.to_string();
        let group_members = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            hunt_group_members::table
                .filter(hunt_group_members::sip_user_uuid.eq(user_uuid))
                .filter(hunt_group_members::deleted.eq(0))
                .load::<GroupMember>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        let groups = group_members
            .map(|members| members.into_iter().map(|m| m.hunt_group_uuid).collect());

        self.put_cache(&key, groups.as_ref()).await;

        groups
    }

    pub async fn get_group(
        &self,
        tenant_uuid: &str,
        group_uuid: &str,
    ) -> Option<Group> {
        let hunt_group = self
            .get_hunt_group(group_uuid)
            .await
            .filter(|g| g.tenant_id == tenant_uuid)?;

        let members = self
            .get_group_users(tenant_uuid, group_uuid)
            .await
            .unwrap_or_default();
        Some(Group {
            uuid: hunt_group.uuid,
            name: hunt_group.name,
            tenant_id: hunt_group.tenant_id,
            members,
        })
    }

    pub async fn update_queue_record(
        &self,
        uuid: Uuid,
        change: QueueRecordChange,
    ) -> Result<QueueRecord> {
        let pool = self.pool.clone();
        let cdr_item = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(queue_records::dsl::queue_records.find(&uuid))
                .set(&change)
                .get_result::<QueueRecord>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;
        Ok(cdr_item)
    }

    pub async fn get_cdr(&self, uuid: Uuid) -> Option<CdrItem> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            cdr_items::table
                .filter(cdr_items::uuid.eq(uuid))
                .first::<CdrItem>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn update_cdr(
        &self,
        uuid: Uuid,
        change: CdrItemChange,
    ) -> Result<CdrItem> {
        let pool = self.pool.clone();
        let cdr_item = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::update(cdr_items::dsl::cdr_items.find(&uuid))
                .set(&change)
                .get_result::<CdrItem>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;
        Ok(cdr_item)
    }

    pub async fn get_voicemail_message(&self, id: &str) -> Option<VoicemailMessage> {
        let pool = self.pool.clone();
        let voicemail_id = id.to_string();
        spawn_blocking(move || -> Option<VoicemailMessage> {
            let db_conn = pool.get().ok()?;
            voicemail_messages::table
                .filter(voicemail_messages::uuid.eq(voicemail_id))
                .filter(voicemail_messages::deleted.eq(0))
                .first::<VoicemailMessage>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_voicemail_messages(
        &self,
        mailbox_id: &str,
    ) -> Option<Vec<VoicemailMessage>> {
        let pool = self.pool.clone();
        let mailbox_id = mailbox_id.to_string();
        spawn_blocking(move || -> Option<Vec<VoicemailMessage>> {
            let db_conn = pool.get().ok()?;
            let msgs = voicemail_messages::table
                .filter(voicemail_messages::voicemail_user_uuid.eq(mailbox_id))
                .filter(voicemail_messages::deleted.eq(0))
                .load::<VoicemailMessage>(&db_conn)
                .ok();
            msgs
        })
        .await
        .ok()?
    }

    pub async fn create_voicemail(
        &self,
        uuid: String,
        voicemail_user_uuid: String,
        callerid: String,
        calleeid: String,
        duration: i64,
        time: SystemTime,
    ) -> Result<VoicemailMessage> {
        let pool = self.pool.clone();
        let voicemail_message = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let new_voicemail_message = NewVoicemailMessage {
                uuid,
                voicemail_user_uuid,
                deleted: 0,
                callerid,
                calleeid,
                folder: "new".to_string(),
                duration,
                time,
            };
            diesel::insert_into(voicemail_messages::table)
                .values(&new_voicemail_message)
                .get_result::<VoicemailMessage>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(voicemail_message)
    }

    pub async fn create_queue_record(
        &self,
        queue_id: String,
        group_id: String,
    ) -> Result<QueueRecord> {
        let pool = self.pool.clone();
        let queue_record = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let start = Utc::now();
            let new_queue_record = NewQueueRecord {
                start,
                queue_id,
                group_id,
                disposition: "ongoing".to_string(),
            };
            diesel::insert_into(queue_records::table)
                .values(&new_queue_record)
                .get_result::<QueueRecord>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(queue_record)
    }

    pub async fn create_queue_failed_user(
        &self,
        start: DateTime<Utc>,
        group_id: String,
        user_id: String,
        failed_times: i64,
        queue_record_id: String,
    ) -> Result<QueueFailedUser> {
        let pool = self.pool.clone();
        let queue_failed_user = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let new_queue_failed_user = NewQueueFailedUser {
                start,
                group_id,
                user_id,
                failed_times,
                queue_record_id,
            };
            diesel::insert_into(queue_failed_users::table)
                .values(&new_queue_failed_user)
                .get_result::<QueueFailedUser>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(queue_failed_user)
    }

    pub async fn create_sound(
        &self,
        uuid: String,
        tenant_id: String,
        name: String,
        duration: f64,
    ) -> Result<Sound> {
        let pool = self.pool.clone();
        let sound = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let new_sound = NewSound {
                uuid,
                tenant_id,
                deleted: 0,
                name,
                time: Utc::now(),
                duration,
            };
            diesel::insert_into(sounds::table)
                .values(&new_sound)
                .get_result::<Sound>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(sound)
    }

    pub async fn create_cdr_full(&self, new_cdr: NewCdrItemFull) -> Result<CdrItem> {
        let pool = self.pool.clone();
        let cdr_item = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::insert_into(cdr_items::table)
                .values(&new_cdr)
                .get_result::<CdrItem>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(cdr_item)
    }

    pub async fn create_cdr(&self) -> Result<CdrItem> {
        let pool = self.pool.clone();
        let cdr_item = spawn_blocking(move || {
            let db_conn = pool.get()?;
            let start = Utc::now();
            let new_cdr_item = NewCdrItem {
                start,
                disposition: "noanswer".to_string(),
                tenant_id: "".to_string(),
                cost: BigDecimal::from_str("0.0")?,
            };
            diesel::insert_into(cdr_items::table)
                .values(&new_cdr_item)
                .get_result::<CdrItem>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(cdr_item)
    }

    pub async fn create_cdr_event(
        &self,
        new_cdr_event: NewCdrEvent,
    ) -> Result<CdrEvent> {
        let pool = self.pool.clone();
        let cdr_event = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::insert_into(cdr_events::table)
                .values(&new_cdr_event)
                .get_result::<CdrEvent>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(cdr_event)
    }

    pub async fn insert_sipuser_aux_log(
        &self,
        user_id: String,
        time: DateTime<Utc>,
        code: String,
        set_by: String,
    ) -> Result<SipuserAuxLog> {
        let pool = self.pool.clone();
        let new_aux_log = NewSipuserAuxLog {
            user_id,
            time,
            code,
            set_by,
        };
        let aux_log = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::insert_into(sipuser_aux_log::table)
                .values(&new_aux_log)
                .get_result::<SipuserAuxLog>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(aux_log)
    }

    pub async fn insert_sipuser_queue_login(
        &self,
        user_id: String,
        time: DateTime<Utc>,
        set_by: String,
        queue_id: Option<String>,
        login: bool,
    ) -> Result<SipuserQueueLoginLog> {
        let pool = self.pool.clone();
        let new_queue_login_log = NewSipuserQueueLoginLog {
            user_id,
            time,
            set_by,
            queue_id,
            login,
        };
        let queue_login_log = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::insert_into(sipuser_queue_login_log::table)
                .values(&new_queue_login_log)
                .get_result::<SipuserQueueLoginLog>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(queue_login_log)
    }

    pub async fn create_fax(&self, new_fax_item: NewFaxItem) -> Result<FaxItem> {
        let pool = self.pool.clone();
        let fax_item = spawn_blocking(move || {
            let db_conn = pool.get()?;
            diesel::insert_into(faxes::table)
                .values(&new_fax_item)
                .get_result::<FaxItem>(&db_conn)
                .map_err(|e| anyhow!(e))
        })
        .await??;

        Ok(fax_item)
    }

    pub async fn get_group_members(
        &self,
        tenant_uuid: &str,
        group_uuid: &str,
    ) -> Option<Vec<GroupMember>> {
        let key = format!("nebula:cache:hunt_group:{group_uuid}:members");

        if let Some(members) = self.get_cache::<Vec<GroupMember>>(&key).await {
            return members;
        }

        let pool = self.pool.clone();
        let local_group_uuid = group_uuid.to_string();
        let group_members = spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            hunt_group_members::table
                .filter(hunt_group_members::hunt_group_uuid.eq(local_group_uuid))
                .filter(hunt_group_members::deleted.eq(0))
                .load::<GroupMember>(&db_conn)
                .ok()
        })
        .await
        .ok()?;

        self.put_cache(&key, group_members.as_ref()).await;
        group_members
    }

    pub async fn get_group_users(
        &self,
        tenant_uuid: &str,
        group_uuid: &str,
    ) -> Option<Vec<User>> {
        let group_members = self
            .get_group_members(tenant_uuid, group_uuid)
            .await
            .unwrap_or_default();

        Some(
            stream::iter(group_members)
                .filter_map(|member| async move {
                    self.get_user(tenant_uuid, &member.sip_user_uuid).await
                })
                .collect()
                .await,
        )
    }

    pub async fn get_aux_log(
        &self,
        user_id: &str,
        start: DateTime<FixedOffset>,
        end: Option<DateTime<FixedOffset>>,
    ) -> Option<Vec<SipuserAuxLog>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            let mut query = sipuser_aux_log::table
                .filter(sipuser_aux_log::user_id.eq(user_id))
                .filter(sipuser_aux_log::time.ge(start))
                .into_boxed();
            if let Some(end) = end {
                query = query.filter(sipuser_aux_log::time.le(end));
            }
            query
                .order(sipuser_aux_log::time.desc())
                .load::<SipuserAuxLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_last_aux_log(
        &self,
        user_id: &str,
        time: DateTime<FixedOffset>,
    ) -> Option<SipuserAuxLog> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;

            sipuser_aux_log::table
                .filter(sipuser_aux_log::user_id.eq(user_id))
                .filter(sipuser_aux_log::time.lt(time))
                .order(sipuser_aux_log::time.desc())
                .first::<SipuserAuxLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    pub async fn get_queue_login_log(
        &self,
        user_id: &str,
        start: DateTime<FixedOffset>,
        end: Option<DateTime<FixedOffset>>,
    ) -> Option<Vec<SipuserQueueLoginLog>> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;
            let mut query = sipuser_queue_login_log::table
                .filter(sipuser_queue_login_log::user_id.eq(user_id))
                .filter(sipuser_queue_login_log::time.ge(start))
                .into_boxed();
            if let Some(end) = end {
                query = query.filter(sipuser_queue_login_log::time.le(end));
            }
            query
                .order(sipuser_queue_login_log::time.desc())
                .load::<SipuserQueueLoginLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }

    /*
    In this instance, we only care about the last time the user logged in / out
     of all queues.
    */
    pub async fn get_last_queue_login_log(
        &self,
        user_id: &str,
        time: DateTime<FixedOffset>,
    ) -> Option<SipuserQueueLoginLog> {
        let pool = self.pool.clone();
        let user_id = user_id.to_string();
        spawn_blocking(move || {
            let db_conn = pool.get().ok()?;

            sipuser_queue_login_log::table
                .filter(sipuser_queue_login_log::user_id.eq(user_id))
                .filter(sipuser_queue_login_log::time.lt(time))
                .filter(sipuser_queue_login_log::queue_id.is_null())
                .first::<SipuserQueueLoginLog>(&db_conn)
                .ok()
        })
        .await
        .ok()?
    }
}

pub async fn is_channel_hangup(channel: &str) -> bool {
    let key = &format!("nebula:channel:{}", channel);
    if !REDIS.exists(key).await.unwrap_or(false) {
        return true;
    }

    if REDIS.hexists(key, "hangup").await.unwrap_or(false) {
        return true;
    }

    let _ = REDIS.sadd(ALL_CHANNELS, channel).await;
    false
}

pub async fn is_channel_answered(channel: &str) -> bool {
    let key = &format!("nebula:channel:{}", channel);
    REDIS
        .hget(key, "answered")
        .await
        .unwrap_or_else(|_| "".to_string())
        == "yes"
}

pub async fn user_dialogs(user_uuid: &str) -> Result<Vec<DialogInfo>> {
    let key = &format!("nebula:sipuser:{user_uuid}:channels");
    let channels: Vec<String> = REDIS.smembers(key).await?;

    let mut dialogs = Vec::new();
    for channel in channels {
        if is_channel_hangup(&channel).await {
            let _ = REDIS.srem(key, &channel).await;
            continue;
        }
        if let Ok(dialog) = channel_dialog_info(&channel).await {
            dialogs.push(dialog);
        }
    }
    dialogs.sort_by_key(|d| -d.time.timestamp());
    Ok(dialogs)
}

async fn channel_dialog_info(channel: &str) -> Result<DialogInfo> {
    let s: String = REDIS
        .hget(&format!("nebula:channel:{}", channel), "call_context")
        .await?;
    let context: CallContext = serde_json::from_str(&s).map_err(|e| anyhow!(e))?;
    let local = context.endpoint.extension();
    let remote = context.to.clone();
    let state = if is_channel_answered(channel).await {
        DialogState::Confirmed
    } else {
        DialogState::Early
    };
    let queue = channel_dialog_call_queue(channel).await;
    let call_queue_id = queue.as_ref().map(|(id, _)| id.clone());
    let call_queue_name = queue.as_ref().map(|(_, name)| name.clone());
    Ok(DialogInfo {
        local,
        remote,
        direction: context.direction.clone(),
        callid: channel.to_string(),
        time: context.answred_at.unwrap_or(context.started_at),
        call_queue_id,
        call_queue_name,
        state,
    })
}

async fn channel_dialog_call_queue(channel: &str) -> Option<(String, String)> {
    let inbound_channel_id: String = REDIS
        .hget(&format!("nebula:channel:{channel}"), "inbound_channel")
        .await
        .unwrap_or("".to_string());
    if inbound_channel_id.is_empty() {
        return None;
    }

    let s: String = REDIS
        .hget(
            &format!("nebula:channel:{inbound_channel_id}"),
            "call_context",
        )
        .await
        .ok()?;
    let context: CallContext =
        serde_json::from_str(&s).map_err(|e| anyhow!(e)).ok()?;
    Some((context.call_queue_id?, context.call_queue_name?))
}

fn get_phone_number(exten: &str) -> Option<phonenumber::PhoneNumber> {
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

fn format_national_number(number: &phonenumber::PhoneNumber) -> String {
    number
        .format()
        .mode(phonenumber::Mode::National)
        .to_string()
        .replace([' ', '(', ')'], "")
}
