use anyhow::Result;
use reqwest::{self};
use serde::{Deserialize, Serialize};
use serde_json;

pub struct YaypiClient {
    base_url: String,
    call_auth_url: String,
    auth_token: String,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct YaypiCall {
    uuid: String,
    reseller_uuid: String,
    country_code: u16,
    destination: u64,
    call_duration: usize,
    trunk: bool,
    ip_address: String,
    start_time: Option<usize>,
    completed: Option<bool>,
    user_uuid: Option<String>,
    callerid: Option<String>,
}

impl Default for YaypiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl YaypiClient {
    pub fn new() -> YaypiClient {
        YaypiClient {
            base_url: "http://10.132.0.107:8080/internal".to_string(),
            call_auth_url: "http://10.132.0.172:8080/internal".to_string(),
            auth_token: "567d6543b7254e7fa4103bd876e49b60".to_string(),
        }
    }

    pub async fn update_call(
        &self,
        call_uuid: &str,
        duration: usize,
        start: u64,
        country_code: u16,
        national_number: u64,
        e164: &str,
    ) -> Result<(u16, serde_json::Value)> {
        let res = reqwest::Client::new()
            .put(&format!("{}/call/{}", self.call_auth_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": call_uuid,
                "call_duration": duration,
                "start_time": start,
                "country_code": country_code,
                "destination": national_number,
                "e164": e164,
                "completed": false,
            }))
            .send()
            .await?;
        let code = res.status().as_u16();
        Ok((code, res.json().await?))
    }

    pub async fn create_cdr(&self, tenant_id: &str, call_uuid: &str) -> Result<()> {
        let res = reqwest::Client::new()
            .post(&format!("{}/cdr/{}", self.base_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "reseller_uuid": tenant_id,
            }))
            .send()
            .await?;
        println!("create cdr {}", res.status());
        Ok(())
    }

    pub async fn complete_cdr(
        &self,
        tenant_id: &str,
        call_uuid: &str,
    ) -> Result<()> {
        let res = reqwest::Client::new()
            .put(&format!("{}/cdr/{}", self.base_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "reseller_uuid": tenant_id,
                "completed": true,
            }))
            .send()
            .await?;
        println!("complete cdr {}", res.status());
        Ok(())
    }

    pub async fn call_recording_finished(
        &self,
        call_uuid: &str,
        tenant_id: &str,
    ) -> Result<()> {
        let res = reqwest::Client::new()
            .post(&format!("{}/recording_saved/{}", self.base_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "reseller_uuid": tenant_id,
            }))
            .send()
            .await?;
        println!("recording_saved {}", res.status());
        Ok(())
    }

    pub async fn answer_cdr(&self, tenant_id: &str, call_uuid: &str) -> Result<()> {
        let res = reqwest::Client::new()
            .put(&format!("{}/cdr/{}", self.base_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "reseller_uuid": tenant_id,
                "answered": true,
            }))
            .send()
            .await?;
        println!("answer cdr {}", res.status());
        Ok(())
    }

    pub async fn complete_call(
        &self,
        call_uuid: &str,
        duration: usize,
        start: u64,
        country_code: u16,
        national_number: u64,
        e164: &str,
        provider_name: String,
    ) -> Result<(u16, serde_json::Value)> {
        let res = reqwest::Client::new()
            .put(&format!("{}/call/{}", self.call_auth_url, call_uuid))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": call_uuid,
                "call_duration": duration,
                "start_time": start,
                "country_code": country_code,
                "destination": national_number,
                "e164": e164,
                "completed": true,
                "provider": provider_name
            }))
            .send()
            .await?;
        let code = res.status().as_u16();
        let result = res.json().await?;
        println!("complete call {:?}", result);
        Ok((code, result))
    }

    pub async fn voicemail_msg_delete(
        &self,
        voicemail_user_uuid: &str,
        uuid: &str,
    ) -> Result<()> {
        let _res = reqwest::Client::new()
            .delete(&format!(
                "{}/voicemail/{}/{}",
                self.base_url, voicemail_user_uuid, uuid
            ))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": uuid,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn notify_fax(&self, tenant_id: &str, uuid: &str) -> Result<()> {
        println!("fax notify {}", uuid);
        let res = reqwest::Client::new()
            .post(&format!("{}/fax/inbound", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": uuid,
                "reseller_uuid": tenant_id,
            }))
            .send()
            .await?
            .text()
            .await?;
        println!("fax notify {} {}", uuid, res);
        Ok(())
    }

    pub async fn voicemail_msg_notify(
        &self,
        voicemail_user_uuid: &str,
        uuid: &str,
    ) -> Result<()> {
        println!("vocemail notify {}", uuid);
        let _res = reqwest::Client::new()
            .post(&format!(
                "{}/voicemail/{}",
                self.base_url, voicemail_user_uuid
            ))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": uuid,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn voicemail_msg_notify_saved(&self, uuid: &str) -> Result<()> {
        println!("vocemail notify saved {}", uuid);
        let _res = reqwest::Client::new()
            .post(&format!("{}/voicemail/{uuid}/seen", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": uuid,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn create_sound(
        &self,
        sound_uuid: &str,
        tenant_id: &str,
        name: &str,
    ) -> Result<()> {
        let _res = reqwest::Client::new()
            .post(&format!("{}/audio", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "tenant_id": tenant_id,
                "audio_uuid": sound_uuid,
                "name": name,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn notify_inbound_limit(&self, number_id: &str) -> Result<()> {
        let _res = reqwest::Client::new()
            .post(&format!("{}/inbound_limit", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "number": number_id,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn notify_incoming(
        &self,
        call_uuid: &str,
        tenant_id: &str,
        from: &str,
        to: &str,
    ) -> Result<()> {
        let _res = reqwest::Client::new()
            .post(&format!("{}/incoming", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "uuid": call_uuid,
                "tenant_id": tenant_id,
                "from": from,
                "to": to,
            }))
            .send()
            .await?;
        Ok(())
    }

    pub async fn create_call(
        &self,
        request_json: serde_json::Value,
    ) -> Result<(u16, serde_json::Value)> {
        let res = reqwest::Client::new()
            .post(&format!("{}/call", self.call_auth_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&request_json)
            .send()
            .await?;
        let code = res.status().as_u16();
        Ok((code, res.json().await?))
    }

    pub async fn check_outbound_restriction(
        &self,
        request_json: serde_json::Value,
    ) -> Result<(u16, serde_json::Value)> {
        let res = reqwest::Client::new()
            .post(&format!("{}/call-outbound-test", self.call_auth_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&request_json)
            .send()
            .await?;
        let code = res.status().as_u16();
        Ok((code, res.json().await?))
    }

    pub async fn notify_voicemail_pin(
        &self,
        voicemail_uuid: String,
        new_pin: String,
    ) -> Result<()> {
        reqwest::Client::new()
            .put(&format!(
                "{}/voicemail/{}/pin",
                self.base_url, voicemail_uuid
            ))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .body(format!(r#"{{"password": "{}"}}"#, new_pin))
            .send()
            .await?;
        Ok(())
    }

    pub async fn create_cdr_event(
        &self,
        request_json: serde_json::Value,
    ) -> Result<()> {
        let res = reqwest::Client::new()
            .post(&format!("{}/user_ringing", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&request_json)
            .send()
            .await?;
        let code = res.status().as_u16();
        println!("create cdr event {code}");
        Ok(())
    }

    pub async fn assign_call_route(
        &self,
        from_user: Option<&str>,
        from_trunk: Option<&str>,
        number_uuid: &str,
        call_route: &str,
        diray: &str,
    ) -> Result<()> {
        let res = reqwest::Client::new()
            .post(&format!("{}/assign_call_route", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "from_user": from_user,
                "from_trunk": from_trunk,
                "number": number_uuid,
                "call_route": call_route,
                "diary": diray,
            }))
            .send()
            .await?;
        println!("assign call route {}", res.status());
        Ok(())
    }

    pub async fn toggle_hunt_group_member(
        &self,
        user: &str,
        group: &str,
    ) -> Result<(bool, Option<String>)> {
        let res: serde_json::Value = reqwest::Client::new()
            .put(&format!(
                "{}/voip/user/{user}/toggle-group/{group}",
                self.base_url
            ))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .send()
            .await?
            .json()
            .await?;

        if let Some(err_tts) = res.get("error_tts").and_then(|r| r.as_str()) {
            return Ok((false, Some(err_tts.to_string())));
        }

        let result = res
            .get("result")
            .and_then(|r| r.as_bool())
            .ok_or_else(|| anyhow::anyhow!("resp doesn't have result"))?;

        Ok((result, None))
    }

    pub async fn new_hot_desk(
        &self,
        from: &str,
        to: &str,
        user_agent: Option<&str>,
    ) -> Result<bool> {
        let res = reqwest::Client::new()
            .post(&format!("{}/hot-desk", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "from": from,
                "to": to,
                "user_agent": user_agent,
            }))
            .send()
            .await?;
        let result: serde_json::Value = res.json().await?;
        let login = result
            .get("login")
            .and_then(|l| l.as_bool())
            .unwrap_or(false);
        Ok(login)
    }

    pub async fn hot_desk_logout(
        &self,
        from: &str,
        user_agent: Option<&str>,
    ) -> Result<()> {
        let res = reqwest::Client::new()
            .post(&format!("{}/hot-desk", self.base_url))
            .header("X-Auth-Token", &self.auth_token)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "from": from,
                "login": false,
                "user_agent": user_agent,
            }))
            .send()
            .await?;
        println!("hot desk logout {}", res.status());
        Ok(())
    }
}
