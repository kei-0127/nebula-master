use crate::server::PRPOXY_SERVICE;
use anyhow::{Error, Result};
use nebula_db::message::UserAgentType;
use nebula_db::models::{is_valid_number, Number};
use nebula_db::{message::Endpoint, models::Provider};
use phonenumber;
use rand::distributions::Alphanumeric;
use rand::Rng;
use sip::message;
use sip::message::{Message, MessageError, Method};
use sip::transport::TransportType;
use std::net::ToSocketAddrs;

pub struct Auth {}

pub(crate) async fn get_internal_number(exten: &str) -> Option<Number> {
    let number = get_phone_number(exten)?;
    let e164 = number.format().mode(phonenumber::Mode::E164).to_string();
    PRPOXY_SERVICE
        .db
        .get_number("admin", &e164[1..].to_string())
        .await
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

    let number = phonenumber::parse(Some(phonenumber::country::GB), exten).ok()?;
    if !is_valid_number(&number) {
        return None;
    }
    Some(number)
}

impl Default for Auth {
    fn default() -> Self {
        Self::new()
    }
}

impl Auth {
    pub fn new() -> Auth {
        Auth {}
    }

    pub async fn get_endpoint(&self, msg: &Message) -> Option<Endpoint> {
        let host = &msg.remote_uri.as_ref()?.host;

        if let Some(internal_ips) = PRPOXY_SERVICE.config.internal_ips.as_ref() {
            if internal_ips.contains(host) {
                return None;
            }
        }

        if (msg.from.uri.host.ends_with("pstnhub.microsoft.com")
            || msg
                .contact
                .as_ref()
                .map(|c| c.uri.host.ends_with("pstnhub.microsoft.com"))
                .unwrap_or(false))
            && "sip-all.pstnhub.microsoft.com:443"
                .to_socket_addrs()
                .ok()?
                .map(|a| a.ip().to_string())
                .any(|i| &i == host)
        {
            let msteam_domain = &msg.request_uri.as_ref()?.host;

            if msg.replaces.is_some() {
                let user = PRPOXY_SERVICE
                    .db
                    .get_msteam_user_in_account(msteam_domain)
                    .await?;
                return Some(Endpoint::User {
                    user,
                    group: None,
                    user_agent: None,
                    user_agent_type: Some(UserAgentType::MsTeams),
                });
            }

            let msteam = msg.from.uri.user.as_ref()?;
            let user = match PRPOXY_SERVICE
                .db
                .get_user_by_msteam(msteam, msteam_domain)
                .await
            {
                Some(user) => user,
                None => {
                    // when Teams send in a call for blind transfer
                    // the from address wouldn't be the msteam of the user who
                    // initiated the transfer, and it's the caller id.
                    // So we have to pick any user in the account to make the call
                    PRPOXY_SERVICE
                        .db
                        .get_msteam_user_in_account(msteam_domain)
                        .await?
                }
            };

            return Some(Endpoint::User {
                user,
                group: None,
                user_agent: None,
                user_agent_type: Some(UserAgentType::MsTeams),
            });
        }

        if host == "104.199.110.107" || host == "35.187.44.195" {
            if let Some(device) = msg.device.as_ref() {
                if let Some(device_token) = msg.device_token.as_ref() {
                    let user = PRPOXY_SERVICE
                        .db
                        .get_user_by_device_token(device, device_token)
                        .await?;
                    return Some(Endpoint::User {
                        user,
                        group: None,
                        user_agent: None,
                        user_agent_type: Some(UserAgentType::MobileApp),
                    });
                }
            }
        }

        if host == "104.199.9.146"
            || host == "23.251.134.254"
            || host == "104.155.107.76"
            || host == "130.211.62.248"
            || host == "35.190.222.244"
        {
            if let Some(user) = msg.auth_user.as_ref() {
                let user = PRPOXY_SERVICE.db.get_user_by_name(user).await?;
                if user.trunk {
                    let trunk_auth =
                        PRPOXY_SERVICE.db.get_trunk_by_user(&user.uuid).await?;
                    let trunk = PRPOXY_SERVICE
                        .db
                        .get_trunk(&trunk_auth.tenant_id, &trunk_auth.trunk_uuid)
                        .await?;
                    let (number, anonymous) = format_number(
                        msg,
                        trunk.display_name_as_callerid.unwrap_or(false),
                    );
                    return Some(Endpoint::Trunk {
                        trunk,
                        number,
                        anonymous,
                        extension: msg.trunk_extension.clone().unwrap_or_default(),
                        ziron_tag: msg.ziron_tag.clone(),
                    });
                }
                return Some(Endpoint::User {
                    user,
                    group: None,
                    user_agent: None,
                    user_agent_type: None,
                });
            }
            if let Some(provider) = msg.auth_provider.as_ref() {
                let (number, anonymous) = format_number(msg, false);
                let tenant_id =
                    get_internal_number(msg.request_uri.as_ref()?.user.as_ref()?)
                        .await
                        .and_then(|n| n.tenant_id);
                return Some(Endpoint::Provider {
                    provider: Provider {
                        id: 0,
                        name: provider.to_string(),
                        host: Some(provider.to_string()),
                        ..Default::default()
                    },
                    number,
                    anonymous,
                    tenant_id,
                    diversion: msg.diversion.clone(),
                    pai: msg.pai.clone(),
                });
            }
        }
        if let Some(provider) = PRPOXY_SERVICE
            .db
            .get_provider(&msg.remote_uri.as_ref()?.host)
            .await
        {
            let (number, anonymous) = format_number(msg, false);
            let tenant_id =
                get_internal_number(msg.request_uri.as_ref()?.user.as_ref()?)
                    .await
                    .and_then(|n| n.tenant_id);
            return Some(Endpoint::Provider {
                provider,
                number,
                anonymous,
                tenant_id,
                diversion: msg.diversion.clone(),
                pai: msg.pai.clone(),
            });
        }
        if let Some(trunk_auth) = PRPOXY_SERVICE
            .db
            .get_trunk_by_ip(
                &msg.remote_uri.as_ref()?.host,
                msg.trunk_info.as_deref().unwrap_or(""),
            )
            .await
        {
            if let Some(trunk) = PRPOXY_SERVICE
                .db
                .get_trunk(&trunk_auth.tenant_id, &trunk_auth.trunk_uuid)
                .await
            {
                let (number, anonymous) = format_number(
                    msg,
                    trunk.display_name_as_callerid.unwrap_or(false),
                );
                return Some(Endpoint::Trunk {
                    trunk,
                    number,
                    anonymous,
                    extension: msg.trunk_extension.clone().unwrap_or_default(),
                    ziron_tag: msg.ziron_tag.clone(),
                });
            }
        }

        if let Some(request_uri) = msg.request_uri.as_ref() {
            if let Some(user) = request_uri.user.as_ref() {
                if let Some(number) =
                    PRPOXY_SERVICE.db.get_number("admin", user).await
                {
                    if !number.require_auth {
                        let (from_number, from_anonymous) =
                            format_number(msg, false);
                        return Some(Endpoint::Provider {
                            provider: Provider {
                                id: 0,
                                name: "anonymous".to_string(),
                                host: Some(host.clone()),
                                ..Default::default()
                            },
                            number: from_number,
                            anonymous: from_anonymous,
                            tenant_id: number.tenant_id,
                            diversion: msg.diversion.clone(),
                            pai: msg.pai.clone(),
                        });
                    }
                }
                if let Some(user) = PRPOXY_SERVICE.db.get_user_by_name(user).await {
                    if !user.require_auth {
                        let (from_number, from_anonymous) =
                            format_number(msg, false);
                        return Some(Endpoint::Provider {
                            provider: Provider {
                                id: 0,
                                name: "anonymous".to_string(),
                                host: Some(host.clone()),
                                ..Default::default()
                            },
                            number: from_number,
                            anonymous: from_anonymous,
                            tenant_id: Some(user.tenant_id),
                            diversion: msg.diversion.clone(),
                            pai: msg.pai.clone(),
                        });
                    }
                }
            }
        }

        let authorization = match msg.method.as_ref().unwrap_or(&Method::INVITE) {
            &Method::REGISTER => msg.authorization.as_ref(),
            _ => msg.proxy_authorization.as_ref(),
        }?;

        let user = if let Some(prefix) =
            authorization.realm.strip_suffix(".hosted.surevoip.co.uk")
        {
            PRPOXY_SERVICE
                .db
                .get_user_by_name(&format!("{}{prefix}", authorization.username))
                .await?
        } else {
            PRPOXY_SERVICE
                .db
                .get_user_by_name(&authorization.username)
                .await?
        };

        let resp = Self::digest(
            &authorization.username,
            &user.password,
            &authorization.realm,
            &authorization.uri,
            &msg.method.as_ref().unwrap_or(&Method::INVITE).to_string(),
            &authorization.nonce,
            authorization.qop.as_deref(),
            authorization.cnonce.as_deref(),
            authorization.nonce_count.as_deref(),
        );
        if resp == authorization.response {
            if user.trunk {
                let trunk_auth =
                    PRPOXY_SERVICE.db.get_trunk_by_user(&user.uuid).await?;
                let trunk = PRPOXY_SERVICE
                    .db
                    .get_trunk(&trunk_auth.tenant_id, &trunk_auth.trunk_uuid)
                    .await?;
                let (number, anonymous) = format_number(
                    msg,
                    trunk.display_name_as_callerid.unwrap_or(false),
                );
                return Some(Endpoint::Trunk {
                    trunk,
                    number,
                    anonymous,
                    extension: msg.trunk_extension.clone().unwrap_or_default(),
                    ziron_tag: msg.ziron_tag.clone(),
                });
            }
            return Some(Endpoint::User {
                user,
                group: None,
                user_agent: msg.user_agent.clone(),
                user_agent_type: msg.remote_uri.as_ref().map(|uri| {
                    if uri.transport == TransportType::Ws
                        || uri.transport == TransportType::Wss
                    {
                        UserAgentType::DesktopApp
                    } else {
                        UserAgentType::Other
                    }
                }),
            });
        }

        None
    }

    pub fn challenge(&self, msg: &Message) -> Result<Message, Error> {
        let mut resp = Message::default();

        let method = msg.method.as_ref().ok_or(MessageError::NotRequest)?;

        let realm = msg.to.uri.host.clone();
        let nonce = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(20)
            .collect::<String>();
        let auth = message::Authenticate { realm, nonce };

        if method == &Method::REGISTER {
            resp.code = Some(401);
            resp.status = Some("Unauthorized".to_string());
            resp.www_authenticate = Some(auth);
        } else {
            resp.code = Some(407);
            resp.status = Some("Proxy Authentication Required".to_string());
            resp.proxy_authenticate = Some(auth);
        }

        resp.via = msg.via.clone();
        resp.to = msg.to.clone();
        resp.from = msg.from.clone();
        resp.callid = msg.callid.clone();
        resp.cseq = msg.cseq.clone();

        Ok(resp)
    }

    pub fn digest(
        username: &str,
        password: &str,
        realm: &str,
        uri: &str,
        method: &str,
        nonce: &str,
        qop: Option<&str>,
        cnonce: Option<&str>,
        nc: Option<&str>,
    ) -> String {
        let a1 = format!("{}:{}:{}", username, realm, password);
        let a2 = format!("{}:{}", method, uri);

        if let Some(cnonce) = cnonce {
            nebula_utils::md5(&format!(
                "{}:{}:{}:{}:{}:{}",
                nebula_utils::md5(&a1),
                nonce,
                nc.unwrap_or(""),
                cnonce,
                qop.unwrap_or(""),
                nebula_utils::md5(&a2)
            ))
        } else {
            nebula_utils::md5(&format!(
                "{}:{}:{}",
                nebula_utils::md5(&a1),
                nonce,
                nebula_utils::md5(&a2)
            ))
        }
    }
}

fn format_number(msg: &Message, display_name_as_callerid: bool) -> (String, bool) {
    let number = if display_name_as_callerid {
        msg.from.display_name.clone()
    } else {
        msg.from.uri.user.clone().unwrap_or_default()
    };

    let is_anonymous = number.to_lowercase() == "anonymous";
    if is_anonymous {
        return ("anonymous".to_string(), true);
    }

    let (number, anonymous) = if let Some(number) = get_phone_number(&number) {
        (
            number.format().mode(phonenumber::Mode::E164).to_string(),
            is_anonymous,
        )
    } else {
        (number, true)
    };

    (number, anonymous)
}

#[cfg(test)]
mod tests {
    use super::get_phone_number;

    #[test]
    fn test_get_phone_number() {
        let n = get_phone_number("52180002080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );

        let n = get_phone_number("52180102080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );

        let n = get_phone_number("50404302080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );

        let n = get_phone_number("50504302080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );

        let n = get_phone_number("442080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );

        let n = get_phone_number("02080908160").unwrap();
        assert_eq!(
            n.format().mode(phonenumber::Mode::E164).to_string(),
            "+442080908160"
        );
    }
}
