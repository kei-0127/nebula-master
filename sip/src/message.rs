use super::transport::TransportType;
use anyhow::{anyhow, Error, Result};
use indexmap::IndexMap;
use nebula_utils::sha1;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::default::Default;
use std::fmt;
use std::fmt::Display;
use std::hash::Hash;
use std::io;
use std::io::{BufRead, BufReader, Read};
use std::str;
use std::str::FromStr;
use strum_macros;
use strum_macros::EnumString;
use thiserror::Error;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader as AsyncBufReader,
};
use url::form_urlencoded;

lazy_static::lazy_static! {
    static ref URI_REGEX: Regex = Regex::new(
            [
                r"^(?P<scheme>[a-zA-Z][a-zA-Z0-9\+\-\.]*):",
                r"(?:(?:(?P<user>[a-zA-Z0-9\-_\.!\~\*\#'\(\)&=\+\$,;\?/%]+)",
                r"(?::(?P<password>[^:@;\?]+))?)@)?",
                r"(?:(?:(?P<host>[^;\?:]*)(?::(?P<port>[\d]+))?))",
                r"(?:;(?P<params>[^\?]*))?",
                r"(?:\?(?P<headers>.*))?$",
            ]
            .concat()
            .as_ref(),
        ).unwrap();

    static ref ADDRESS_REGEX: [Regex;3] = [
        Regex::new(r#"^(?P<name>[a-zA-Z0-9\-\._\+\~ \t]*)<(?P<uri>[^>]+)>(?:;(?P<params>[^\?]*))?"#).unwrap(),
        Regex::new(r#"^(?:"(?P<name>[^"]+)")[ \t]*<(?P<uri>[^>]+)>(?:;(?P<params>[^\?]*))?"#).unwrap(),
        Regex::new(r#"^[ \t]*(?P<name>)(?P<uri>[^;]+)(?:;(?P<params>[^\?]*))?"#).unwrap(),
    ];
}

#[derive(
    strum_macros::Display,
    EnumString,
    Debug,
    PartialEq,
    Eq,
    Clone,
    Serialize,
    Deserialize,
)]
pub enum Method {
    INVITE,
    REGISTER,
    CANCEL,
    ACK,
    BYE,
    SUBSCRIBE,
    REFER,
    INFO,
    NOTIFY,
    PUBLISH,
    OPTIONS,
}

#[derive(Deserialize, Serialize, Debug)]
pub enum HangupReason {
    CompletedElsewhere,
}

impl Default for Method {
    fn default() -> Self {
        Method::INVITE
    }
}

#[derive(Debug, Error)]
pub enum MessageError {
    #[error("message is not Request")]
    NotRequest,
    #[error("message is not Response")]
    NotResponse,
    #[error("via header is not in message")]
    NoVia,
    #[error("invalid message")]
    InvalidMessage,
    #[error("invalid via header")]
    InvalidVia,
    #[error("invalid authorization header")]
    InvalidAuthorization,
    #[error("invalid uri")]
    InvalidUri,
    #[error("invalid address")]
    InvalidAddress,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Cseq {
    pub seq: i32,
    pub method: Method,
}

impl FromStr for Cseq {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(2, ' ').collect();
        if parts.len() != 2 {
            Err(MessageError::InvalidMessage)?;
        }
        let seq = parts[0].parse::<i32>()?;
        let method = Method::from_str(parts[1])?;
        Ok(Cseq { seq, method })
    }
}

impl Display for Cseq {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} {}", self.seq, self.method)
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Via {
    pub transport: TransportType,
    pub host: String,
    pub port: Option<u16>,
    pub branch: String,
    pub received: Option<String>,
    pub rport: Option<u16>,
    pub params: IndexMap<String, Option<String>>,
}

impl FromStr for Via {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut via = Via::default();
        let parts: Vec<&str> = s.splitn(2, ' ').collect();
        if parts.len() != 2 {
            Err(MessageError::InvalidVia)?;
        }

        let proto = parts[0];
        let addr = parts[1];

        let parts: Vec<&str> = proto.split('/').collect();
        if parts.len() != 3 {
            Err(MessageError::InvalidVia)?;
        }
        via.transport = TransportType::from_str(&parts[2].to_lowercase())?;

        let uri = Uri::from_str(&["sip:", addr].concat())?;
        via.host = uri.host;
        via.port = uri.port;
        for (key, value) in uri.params {
            match key.as_ref() {
                "branch" => via.branch = value.unwrap_or_else(|| "".to_string()),
                "received" => via.received = value,
                "rport" => {
                    via.rport = value.map(|r| r.parse::<u16>().unwrap_or(0));
                }
                _ => {
                    via.params.insert(key, value);
                }
            }
        }

        Ok(via)
    }
}

impl Display for Via {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "SIP/2.0/{} {}",
            self.transport.to_string().to_uppercase(),
            self.host
        )?;

        if let Some(ref p) = self.port {
            write!(f, ":{}", p)?;
        }

        if let Some(ref r) = self.received {
            write!(f, ";received={}", r)?;
        }

        if let Some(ref r) = self.rport {
            write!(f, ";rport={}", r)?;
        }

        write!(f, ";branch={}", self.branch)?;

        Ok(())
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CallerId {
    pub user: String,
    pub e164: Option<String>,
    pub display_name: String,
    pub anonymous: bool,
    pub asserted_identity: Option<String>,
    /// The original caller id we received from provider
    /// before applying things like call_flow_show_original_callerid
    pub original_number: Option<String>,
    /// the receving number
    pub to_number: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct InterTenantInfo {
    pub src_tenant_id: String,
    pub dst_tenant_id: String,
    pub exten: String,
    pub from_exten: String,
    pub from_display_name: String,
    pub extern_call_flow: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TeamsReferCx {
    pub channel_id: String,
    pub refer_to: Address,
    pub refer_by: Address,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Location {
    pub user: String,
    pub display_name: Option<String>,
    pub from_host: String,
    pub to_host: String,
    pub proxy_host: String,
    pub req_uri: Uri,
    pub dst_uri: Uri,
    pub user_agent: Option<String>,
    pub intra: bool,
    pub encryption: bool,
    pub inter_tenant: Option<InterTenantInfo>,
    pub route: Option<Uri>,
    pub device: Option<String>,
    pub device_token: Option<String>,
    pub dnd: bool,
    pub dnd_allow_internal: bool,
    pub provider: Option<bool>,
    pub is_registration: Option<bool>,
    pub tenant_id: Option<String>,
    pub teams_refer_cx: Option<TeamsReferCx>,
    pub ziron_tag: Option<String>,
    pub diversion: Option<Address>,
    pub pai: Option<Address>,
    pub global_dnd: bool,
    pub global_forward: Option<String>,
    pub room_auth_token: Option<String>,
}

impl Hash for Location {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dst_uri.host.hash(state);
        self.dst_uri.get_port().hash(state);
        self.dst_uri.transport.hash(state);
    }
}

impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        self.dst_uri.host == other.dst_uri.host
            && self.dst_uri.get_port() == other.dst_uri.get_port()
            && self.dst_uri.transport == other.dst_uri.transport
    }
}

impl Eq for Location {}

#[derive(Default, Clone, Deserialize, Serialize, Debug)]
pub struct Uri {
    pub scheme: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: String,
    pub ip: String,
    pub port: Option<u16>,
    pub transport: TransportType,
    pub lr: bool,
    pub params: IndexMap<String, Option<String>>,
    pub headers: IndexMap<String, String>,
    // If the URI came from a sip user registration
    // It means that for sip messages sent to this uri,
    // we don't make a new connection if the connection doesn't exist
    pub is_registration: Option<bool>,
    // this is set to Some(true) when the remote ends is a provider,
    // then we will send/recive the udp packet from the secondary udp socket
    // specifically for providers
    pub is_provider: Option<bool>,
    // this is for Teams. When making TLS connection to Teams, we need to
    // present the corrent certificate that matches to the host in the contact
    pub contact_host: Option<String>,
}

impl FromStr for Uri {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut uri = Uri::default();
        let caps = URI_REGEX.captures(s).ok_or(MessageError::InvalidUri)?;
        if let Some(m) = caps.get(1) {
            uri.scheme = m.as_str().to_string();
        }
        if let Some(m) = caps.get(2) {
            uri.user = Some(m.as_str().to_string());
        }
        if let Some(m) = caps.get(3) {
            uri.password = Some(m.as_str().to_string());
        }
        if let Some(m) = caps.get(4) {
            uri.host = m.as_str().to_string();
        }
        if let Some(m) = caps.get(5) {
            uri.port = Some(m.as_str().parse::<u16>()?);
        }

        let mut params = IndexMap::new();
        if let Some(m) = caps.get(6) {
            for p in m.as_str().split(';') {
                let mut parts = p.splitn(2, '=');

                let name = parts.next().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let value = parts.next().map(|i| i.to_string());
                match name {
                    "transport" => {
                        uri.transport = TransportType::from_str(
                            &value.unwrap_or_else(|| "udp".to_string()),
                        )?;
                    }
                    "lr" => uri.lr = true,
                    _ => {
                        params.insert(name.to_string(), value);
                    }
                };
            }
        }
        uri.params = params;

        let mut headers = IndexMap::new();
        if let Some(m) = caps.get(7) {
            for (k, v) in form_urlencoded::parse(m.as_str().as_bytes()) {
                headers.insert(k.to_string(), v.to_string());
            }
        }
        uri.headers = headers;

        Ok(uri)
    }
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}:",
            if !self.scheme.is_empty() {
                &self.scheme
            } else {
                "sip"
            }
        )?;
        let user = match self.user.as_ref() {
            Some(u) => [u.as_ref(), "@"].concat(),
            None => "".to_string(),
        };
        write!(f, "{}{}", user, self.host)?;

        if let Some(ref p) = self.port {
            write!(f, ":{}", p)?;
        }

        if self.transport != TransportType::Udp {
            write!(
                f,
                ";transport={}",
                self.transport.to_string().to_lowercase()
            )?;
        }

        if self.lr {
            write!(f, ";lr")?;
        }

        for (key, val) in self.params.iter() {
            f.write_str(";")?;
            f.write_str(key.as_str())?;
            match val {
                Some(inner) => {
                    f.write_str("=")?;
                    f.write_str(inner.as_str())?;
                }
                None => (),
            };
        }

        let mut encoded = form_urlencoded::Serializer::new(String::new());
        for (k, v) in self.headers.iter() {
            encoded.append_pair(k, v);
        }
        let encoded = encoded.finish();
        if !encoded.is_empty() {
            f.write_str("?")?;
            f.write_str(&encoded)?;
        }

        Ok(())
    }
}

impl Uri {
    pub fn get_port(&self) -> u16 {
        match self.port {
            Some(p) => p,
            None => match self.transport {
                TransportType::Udp => 5060,
                TransportType::Tcp => 5060,
                TransportType::Tls => 5061,
                TransportType::Ws => 80,
                TransportType::Wss => 443,
            },
        }
    }

    pub fn addr(&self) -> String {
        if !self.ip.is_empty() {
            format!("{}:{}", self.ip, self.get_port())
        } else {
            format!("{}:{}", self.host, self.get_port())
        }
    }
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
pub struct GenericHeader {
    pub name: String,
    pub content: String,
}

impl Display for GenericHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", &self.name, &self.content)
    }
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
pub struct Address {
    pub display_name: String,
    pub uri: Uri,
    pub tag: Option<String>,
    pub params: IndexMap<String, Option<String>>,
    pub expires: Option<i32>,
}

impl FromStr for Address {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for re in ADDRESS_REGEX.iter() {
            let mut captures = re.captures_iter(s);
            if let Some(cap) = captures.next() {
                if cap.len() != 4 {
                    continue;
                }

                let display_name = cap
                    .get(1)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "".to_string());
                let uri = Uri::from_str(
                    cap.get(2).ok_or(MessageError::InvalidUri)?.as_str(),
                )?;

                let params = IndexMap::new();

                let mut address = Address {
                    display_name,
                    uri,
                    params,
                    tag: None,
                    expires: None,
                };

                if let Some(m) = cap.get(3) {
                    for part in m.as_str().split(';') {
                        let mut split = part.splitn(2, '=');
                        let name = split.next().unwrap_or("");
                        if name.is_empty() {
                            continue;
                        }
                        let value = split.next().map(|i| i.to_string());
                        match name {
                            "tag" => address.tag = value,
                            "expires" => {
                                address.expires = value
                                    .map(|v| v.parse::<i32>().ok())
                                    .unwrap_or(None);
                            }
                            _ => {
                                address.params.insert(name.to_string(), value);
                            }
                        }
                    }
                }

                return Ok(address);
            }
        }
        Err(MessageError::InvalidAddress)?
    }
}

impl Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if !self.display_name.is_empty() {
            write!(f, r#""{}" "#, self.display_name)?;
        }

        write!(f, "<{}>", self.uri)?;

        if let Some(ref t) = self.tag {
            write!(f, ";tag={}", t)?;
        }

        if let Some(ref t) = self.expires {
            write!(f, ";expires={}", t)?;
        }

        Ok(())
    }
}

#[derive(Default, Debug, Clone, Deserialize, Serialize)]
pub struct Authorization {
    pub username: String,
    pub realm: String,
    pub nonce: String,
    pub uri: String,
    pub response: String,
    pub cnonce: Option<String>,
    pub nonce_count: Option<String>,
    pub qop: Option<String>,
}

impl Display for Authorization {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            r#"Digest username="{}",realm="{}",nonce="{}",uri="{}",response="{}",algorithm=MD5"#,
            self.username, self.realm, self.nonce, self.uri, self.response
        )?;

        if let Some(qop) = self.qop.as_ref() {
            write!(f, ",qop={qop}")?;
        }
        if let Some(cnonce) = self.cnonce.as_ref() {
            write!(f, ",cnonce=\"{cnonce}\"")?;
        }
        if let Some(c) = self.nonce_count.as_ref() {
            write!(f, ",nc={c}")?;
        }

        Ok(())
    }
}

impl FromStr for Authorization {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut auth = Authorization::default();
        let parts: Vec<&str> = s[7..].split(',').collect();
        for mut part in parts {
            part = part.trim();

            let mut split = part.splitn(2, '=');
            let name = split.next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let value = split.next().unwrap_or("").to_string();
            match name {
                "username" => auth.username = value[1..&value.len() - 1].to_string(),
                "realm" => auth.realm = value[1..&value.len() - 1].to_string(),
                "nonce" => auth.nonce = value[1..&value.len() - 1].to_string(),
                "uri" => auth.uri = value[1..&value.len() - 1].to_string(),
                "response" => auth.response = value[1..&value.len() - 1].to_string(),
                "qop" => auth.qop = Some(value),
                "cnonce" => {
                    auth.cnonce = Some(value[1..&value.len() - 1].to_string())
                }
                "nc" => auth.nonce_count = Some(value),
                _ => (),
            }
        }

        Ok(auth)
    }
}

#[derive(Default, Debug, Clone, Deserialize, Serialize)]
pub struct Authenticate {
    pub realm: String,
    pub nonce: String,
}

impl Display for Authenticate {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, r#"Digest realm="{}",nonce="{}""#, self.realm, self.nonce)
    }
}

impl FromStr for Authenticate {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut auth = Authenticate::default();
        let parts: Vec<&str> = s[7..].split(',').collect();
        for mut part in parts {
            part = part.trim();

            let mut split = part.splitn(2, '=');
            let name = split.next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut value = split.next().unwrap_or("").to_string();
            value = value[1..&value.len() - 1].to_string();
            match name {
                "realm" => auth.realm = value,
                "nonce" => auth.nonce = value,
                _ => (),
            }
        }

        Ok(auth)
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub method: Option<Method>,
    pub request_uri: Option<Uri>,
    pub code: Option<i32>,
    pub status: Option<String>,

    pub via: Vec<Via>,
    pub route: Vec<Address>,
    pub record_route: Vec<Address>,
    pub max_forwards: Option<i32>,
    pub contact: Option<Address>,
    pub to: Address,
    pub from: Address,
    pub callid: String,
    pub cseq: Cseq,
    pub expires: Option<i32>,
    pub content_length: i32,
    pub authorization: Option<Authorization>,
    pub proxy_authorization: Option<Authorization>,
    pub www_authenticate: Option<Authenticate>,
    pub proxy_authenticate: Option<Authenticate>,
    pub refer_to: Option<Address>,
    pub refer_by: Option<Address>,
    pub allow: Option<String>,
    pub pai: Option<Address>,
    pub pci: Option<Address>,
    pub privacy: Option<String>,
    pub event: Option<String>,
    pub subscription_state: Option<String>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub reason: Option<String>,
    pub alert_info: Option<String>,
    pub body: Option<String>,
    pub device: Option<String>,
    pub device_token: Option<String>,
    pub in_reply_to: Option<String>,
    pub internal_call: Option<String>,
    pub remote_party_id: Option<Address>,
    pub diversion: Option<Address>,
    pub user_agent: Option<String>,
    pub replaces: Option<String>,
    pub trunk_info: Option<String>,
    pub trunk_extension: Option<String>,
    pub ziron_tag: Option<String>,
    pub global_dnd: Option<String>,
    pub global_forward: Option<String>,
    pub room: Option<String>,
    pub room_auth_token: Option<String>,
    pub transfer_target: Option<String>,
    pub transfer_auto_answer: Option<String>,

    pub auth_user: Option<String>,
    pub auth_provider: Option<String>,
    pub destination: Option<Uri>,

    pub remote_uri: Option<Uri>,

    pub generic_headers: Vec<GenericHeader>,
}

impl FromStr for Message {
    type Err = Error;

    fn from_str(s: &str) -> Result<Message, Error> {
        let mut reader = BufReader::new(s.as_bytes());
        Message::parse(&mut reader)
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_request() {
            write!(
                f,
                "{} {} SIP/2.0\r\n",
                self.method.as_ref().unwrap_or(&Method::INVITE),
                self.request_uri.as_ref().unwrap_or(&Uri::default()),
            )?;
        } else {
            write!(
                f,
                "SIP/2.0 {} {}\r\n",
                self.code.as_ref().unwrap_or(&0),
                self.status.as_ref().unwrap_or(&"".to_string())
            )?;
        }

        for via in self.via.iter() {
            write!(f, "Via: {}\r\n", via)?;
        }

        for address in self.record_route.iter() {
            write!(f, "Record-Route: {}\r\n", address)?;
        }

        for address in self.route.iter() {
            write!(f, "Route: {}\r\n", address)?;
        }

        if let Some(ref i) = self.max_forwards {
            write!(f, "Max-Forwards: {}\r\n", i)?;
        }

        if let Some(ref i) = self.contact {
            write!(f, "Contact: {}\r\n", i)?;
        }

        write!(f, "To: {}\r\n", self.to)?;
        write!(f, "From: {}\r\n", self.from)?;
        write!(f, "Call-ID: {}\r\n", self.callid)?;
        write!(f, "CSeq: {}\r\n", self.cseq)?;

        if let Some(ref i) = self.www_authenticate {
            write!(f, "WWW-Authenticate: {}\r\n", i)?;
        }

        if let Some(ref i) = self.proxy_authenticate {
            write!(f, "Proxy-Authenticate: {}\r\n", i)?;
        }

        if let Some(ref i) = self.authorization {
            write!(f, "Authorization: {}\r\n", i)?;
        }

        if let Some(ref i) = self.proxy_authorization {
            write!(f, "Proxy-Authorization: {}\r\n", i)?;
        }

        if let Some(ref i) = self.allow {
            write!(f, "Allow: {}\r\n", i)?;
        }

        if let Some(h) = self.remote_party_id.as_ref() {
            write!(f, "Remote-Party-ID: {}\r\n", h)?;
        }

        if let Some(h) = self.diversion.as_ref() {
            write!(f, "Diversion: {}\r\n", h)?;
        }

        if let Some(h) = self.pai.as_ref() {
            write!(f, "P-Asserted-Identity: {}\r\n", h)?;
        }

        if let Some(h) = self.pci.as_ref() {
            write!(f, "P-Charge-Info: {}\r\n", h)?;
        }

        if let Some(h) = self.privacy.as_ref() {
            write!(f, "Privacy: {}\r\n", h)?;
        }

        if let Some(uri) = self.destination.as_ref() {
            write!(f, "X-Destination: {}\r\n", uri)?;
        }

        if let Some(h) = self.device.as_ref() {
            write!(f, "X-Device: {}\r\n", h)?;
        }

        if let Some(h) = self.device_token.as_ref() {
            write!(f, "X-Device-Token: {}\r\n", h)?;
        }

        if let Some(h) = self.event.as_ref() {
            write!(f, "Event: {}\r\n", h)?;
        }

        if let Some(h) = self.subscription_state.as_ref() {
            write!(f, "Subscription-State: {}\r\n", h)?;
        }

        if let Some(h) = self.expires.as_ref() {
            write!(f, "Expires: {}\r\n", h)?;
        }

        if let Some(ref h) = self.content_type {
            write!(f, "Content-Type: {}\r\n", h)?;
        }

        if let Some(ref h) = self.content_disposition {
            write!(f, "Content-Disposition: {}\r\n", h)?;
        }

        if let Some(ref i) = self.reason {
            write!(f, "Reason: {}\r\n", i)?;
        }

        if let Some(ref i) = self.refer_to {
            write!(f, "Refer-To: {}\r\n", i)?;
        }

        if let Some(ref i) = self.refer_by {
            write!(f, "REFERRED-BY: {}\r\n", i)?;
        }

        if let Some(ref i) = self.in_reply_to {
            write!(f, "In-Reply-To: {}\r\n", i)?;
        }

        if let Some(ref i) = self.internal_call {
            write!(f, "X-Internal-Call: {}\r\n", i)?;
        }

        if let Some(ref i) = self.room {
            write!(f, "X-Room-Id: {}\r\n", i)?;
        }

        if let Some(ref i) = self.room_auth_token {
            write!(f, "X-Room-Auth-Token: {}\r\n", i)?;
        }

        if let Some(ref i) = self.global_dnd {
            write!(f, "X-Global-DND: {}\r\n", i)?;
        }

        if let Some(ref i) = self.global_forward {
            write!(f, "X-Global-Forward: {}\r\n", i)?;
        }

        if let Some(ref i) = self.transfer_target {
            write!(f, "X-Transfer-Target: {}\r\n", i)?;
        }

        if let Some(ref i) = self.transfer_auto_answer {
            write!(f, "X-Transfer-Auto-Answer: {}\r\n", i)?;
        }

        if let Some(ref i) = self.trunk_info {
            write!(f, "X-Trunk-Info: {}\r\n", i)?;
        }

        if let Some(ref i) = self.trunk_extension {
            write!(f, "X-Trunk-Extension: {}\r\n", i)?;
        }

        if let Some(ref i) = self.ziron_tag {
            write!(f, "X-Ziron-Tag: {}\r\n", i)?;
        }

        if let Some(ref i) = self.replaces {
            write!(f, "Replaces: {}\r\n", i)?;
        }

        if let Some(ref i) = self.alert_info {
            write!(f, "Alert-Info: {}\r\n", i)?;
        }

        if let Some(ref i) = self.user_agent {
            write!(f, "User-Agent: {}\r\n", i)?;
        }

        for generic_header in self.generic_headers.iter() {
            write!(f, "{}\r\n", generic_header)?;
        }

        match self.body.as_ref() {
            Some(i) => write!(f, "Content-Length: {}\r\n", i.len())?,
            None => write!(f, "Content-Length: 0\r\n")?,
        };

        write!(f, "\r\n")?;

        if let Some(ref i) = self.body {
            write!(f, "{}", i)?;
        }

        Ok(())
    }
}

impl Message {
    fn parse<R: io::Read>(reader: &mut BufReader<R>) -> Result<Message, Error> {
        let mut msg = Message::default();
        let mut line = String::new();
        reader.read_line(&mut line)?;

        line = line.trim().to_string();
        if line.starts_with("SIP/2.0") {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() != 3 {
                Err(MessageError::InvalidMessage)?;
            }
            msg.code = Some(parts[1].parse::<i32>()?);
            msg.status = Some(parts[2].to_string());
        } else {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() != 3 {
                Err(MessageError::InvalidMessage)?;
            }
            msg.method = Some(Method::from_str(parts[0])?);
            msg.request_uri = Some(Uri::from_str(parts[1])?);
        }

        loop {
            line.clear();
            reader.read_line(&mut line)?;
            line = line.trim().to_string();
            if line == "" {
                break;
            }

            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }

            let value = parts[1].trim();
            let _ = Self::parse_line(&mut msg, parts[0], value);
        }

        if msg.content_length > 0 {
            let mut buf = vec![];
            reader
                .take(msg.content_length as u64)
                .read_to_end(&mut buf)?;
            msg.body = Some(str::from_utf8_mut(&mut buf)?.to_string());
        }

        Ok(msg)
    }

    pub async fn resolve_remote_uri(&mut self) {
        if let Some(remote_uri) = self.remote_uri.as_mut() {
            if remote_uri.ip == "" {
                let mut resolved_ip = None;
                if let Ok(mut ips) =
                    tokio::net::lookup_host(&format!("{}:5060", remote_uri.host))
                        .await
                {
                    if let Some(ip) = ips.next() {
                        resolved_ip = Some(ip.ip().to_string());
                    }
                }
                if let Some(ip) = resolved_ip {
                    remote_uri.ip = ip;
                }
            }
        }
    }

    pub async fn async_parse<R: AsyncRead + Unpin>(
        reader: &mut AsyncBufReader<R>,
    ) -> Result<Message> {
        let mut msg = Message::default();
        let mut line = String::new();
        while line.is_empty() {
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                println!("buf reader read eof");
                return Err(anyhow!("connecton eof"));
            }
            line = line.trim().to_string();
        }
        if line.starts_with("SIP/2.0") {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() != 3 {
                Err(MessageError::InvalidMessage)?;
            }
            msg.code = Some(parts[1].parse::<i32>()?);
            msg.status = Some(parts[2].to_string());
        } else {
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() != 3 {
                Err(MessageError::InvalidMessage)?;
            }
            msg.method = Some(Method::from_str(parts[0])?);
            msg.request_uri = Some(Uri::from_str(parts[1])?);
        }

        loop {
            line.clear();
            reader.read_line(&mut line).await?;
            line = line.trim().to_string();
            if line.is_empty() {
                break;
            }

            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }

            let value = parts[1].trim();
            let _ = Self::parse_line(&mut msg, parts[0], value);
        }

        if msg.content_length > 0 {
            let mut buf = vec![];
            reader
                .take(msg.content_length as u64)
                .read_to_end(&mut buf)
                .await?;
            msg.body = Some(str::from_utf8_mut(&mut buf)?.to_string());
        }

        Ok(msg)
    }

    fn parse_line(msg: &mut Message, filed: &str, value: &str) -> Result<()> {
        match filed.to_lowercase().as_ref() {
            "v" | "via" => {
                for part in value.split(',') {
                    if let Ok(v) = Via::from_str(part.trim()) {
                        msg.via.push(v);
                    }
                }
            }
            "record-route" => {
                if let Ok(a) = Address::from_str(value) {
                    msg.record_route.push(a);
                }
            }
            "max-forwards" => {
                msg.max_forwards = value.parse::<i32>().ok();
            }
            "cseq" => {
                msg.cseq = Cseq::from_str(value)?;
            }
            "expires" => {
                msg.expires = value.parse::<i32>().ok();
            }
            "route" => {
                if let Ok(a) = Address::from_str(value) {
                    msg.route.push(a);
                }
            }
            "m" | "contact" => {
                msg.contact = Address::from_str(value).ok();
            }
            "f" | "from" => {
                if let Ok(a) = Address::from_str(value) {
                    msg.from = a;
                }
            }
            "t" | "to" => {
                if let Ok(a) = Address::from_str(value) {
                    msg.to = a;
                }
            }
            "i" | "call-id" => {
                msg.callid = value.to_string();
            }
            "l" | "content-length" => {
                msg.content_length = value.parse::<i32>().unwrap_or(0);
            }
            "content-type" => {
                msg.content_type = Some(value.to_string());
            }
            "allow" => {
                msg.allow = Some(value.to_string());
            }
            "refer-to" => {
                msg.refer_to = Address::from_str(value).ok();
            }
            "referred-by" => {
                msg.refer_by = Address::from_str(value).ok();
            }
            "x-authuser" => {
                msg.auth_user = Some(value.to_string());
            }
            "x-authprovider" => {
                msg.auth_provider = Some(value.to_string());
            }
            "x-device" => {
                msg.device = Some(value.to_string());
            }
            "x-device-token" => {
                msg.device_token = Some(value.to_string());
            }
            "in-reply-to" => {
                msg.in_reply_to = Some(value.to_string());
            }
            "remote-party-id" => {
                msg.remote_party_id = Address::from_str(value).ok();
            }
            "diversion" => {
                msg.diversion = Address::from_str(value).ok();
            }
            "p-asserted-identity" => {
                msg.pai = Address::from_str(value).ok();
            }
            "x-destination" => {
                msg.destination = Uri::from_str(value).ok();
            }
            "x-internal-call" => {
                msg.internal_call = Some(value.to_string());
            }
            "x-room-id" => {
                msg.room = Some(value.to_string());
            }
            "x-room-auth-token" => {
                msg.room_auth_token = Some(value.to_string());
            }
            "x-global-dnd" => {
                msg.global_dnd = Some(value.to_string());
            }
            "x-global-forward" => {
                msg.global_forward = Some(value.to_string());
            }
            "x-transfer-target" => {
                msg.transfer_target = Some(value.to_string());
            }
            "x-transfer-auto-answer" => {
                msg.transfer_auto_answer = Some(value.to_string());
            }
            "x-trunk-info" => {
                msg.trunk_info = Some(value.to_string());
            }
            "x-trunk-extension" => {
                msg.trunk_extension = Some(value.to_string());
            }
            "x-ziron-tag" => {
                msg.ziron_tag = Some(value.to_string());
            }
            "privacy" => {
                msg.privacy = Some(value.to_string());
            }
            "event" => {
                msg.event = Some(value.to_string());
            }
            "user-agent" => {
                msg.user_agent = Some(value.to_string());
            }
            "alert-info" => {
                msg.alert_info = Some(value.to_string());
            }
            "replaces" => {
                msg.replaces = Some(value.to_string());
            }
            "subscription-state" => {
                msg.subscription_state = Some(value.to_string());
            }
            "www-authenticate" => {
                msg.www_authenticate = Authenticate::from_str(value).ok();
            }
            "proxy-authenticate" => {
                msg.proxy_authenticate = Authenticate::from_str(value).ok();
            }
            "authorization" => {
                msg.authorization = Authorization::from_str(value).ok();
            }
            "proxy-authorization" => {
                msg.proxy_authorization = Authorization::from_str(value).ok();
            }
            _ => msg.generic_headers.push(GenericHeader {
                name: filed.to_string(),
                content: value.to_string(),
            }),
        }
        Ok(())
    }

    pub fn add_generic_header(&mut self, name: String, content: String) {
        self.generic_headers.push(GenericHeader { name, content });
    }

    pub fn is_request(&self) -> bool {
        self.request_uri.is_some()
    }

    pub fn is_invite(&self) -> bool {
        if self.is_request() {
            match &self.method {
                Some(m) => m == &Method::INVITE,
                None => false,
            }
        } else {
            self.cseq.method == Method::INVITE
        }
    }

    pub fn dialog_id(&self) -> Option<String> {
        let tag1 = self.from.tag.as_ref()?;
        let tag2 = self.to.tag.as_ref()?;
        let mut tags = vec![tag1, tag2];
        tags.sort();
        Some(sha1(&format!("{}{}{}", &self.callid, tags[0], tags[1])))
    }

    pub fn dest(&self) -> Result<Uri, Error> {
        if let Some(remote_uri) = &self.remote_uri {
            return Ok(remote_uri.clone());
        }

        if !self.route.is_empty() {
            return Ok(self.route[0].uri.clone());
        }

        Ok(self
            .request_uri
            .as_ref()
            .ok_or(MessageError::NotRequest)?
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn via_from_string() {
        let s = "SIP/2.0/UDP 10.0.0.2;branch=testbranch";
        let via = Via::from_str(s).unwrap();
        assert_eq!(s, via.to_string());

        let s = "SIP/2.0/UDP 10.0.0.2:5080;received=10.0.0.3;rport=5090;branch=testbranch";
        let via = Via::from_str(s).unwrap();
        assert_eq!(s, via.to_string());
        assert_eq!("10.0.0.3", via.received.unwrap());
        assert_eq!(5090, via.rport.unwrap());
    }

    #[test]
    fn via_to_string() {
        let via = Via {
            host: "10.0.0.2".to_string(),
            branch: "testbranch".to_string(),
            ..Default::default()
        };
        assert_eq!("SIP/2.0/UDP 10.0.0.2;branch=testbranch", via.to_string());

        let via = Via {
            host: "10.0.0.2".to_string(),
            port: Some(5080),
            branch: "testbranch".to_string(),
            ..Default::default()
        };
        assert_eq!(
            "SIP/2.0/UDP 10.0.0.2:5080;branch=testbranch",
            via.to_string()
        );

        let via = Via {
            host: "10.0.0.2".to_string(),
            port: Some(5080),
            received: Some("10.0.0.3".to_string()),
            rport: Some(5090),
            branch: "testbranch".to_string(),
            ..Default::default()
        };
        assert_eq!(
            "SIP/2.0/UDP 10.0.0.2:5080;received=10.0.0.3;rport=5090;branch=testbranch",
            via.to_string()
        );
    }

    #[test]
    fn address_from_string() {
        let s = r#""Test Address" <sip:test@example.net>;tag=tag"#;
        let address = Address::from_str(s).unwrap();
        assert_eq!(s, address.to_string());
        assert_eq!("tag", address.tag.unwrap_or_else(|| "".to_string()));

        let s = r#"<sip:test@example.net>;expires=3600"#;
        let address = Address::from_str(s).unwrap();
        assert_eq!(s, address.to_string());

        let s = r#"Test Address<sip:test@example.net>;tag=tag"#;
        let address = Address::from_str(s).unwrap();
        assert_eq!(
            r#""Test Address" <sip:test@example.net>;tag=tag"#,
            address.to_string()
        );

        let s = r#"sip:test@example.net"#;
        let address = Address::from_str(s).unwrap();
        assert_eq!("<sip:test@example.net>", address.to_string());

        let s = r#"<sip:g8h3u4aq@kub2s5580lu4.invalid;transport=ws;ob>"#;
        let address = Address::from_str(s).unwrap();
        assert_eq!(
            "<sip:g8h3u4aq@kub2s5580lu4.invalid;transport=ws;ob>",
            address.to_string()
        );
    }

    #[test]
    fn address_to_string() {
        let address = Address {
            display_name: ("Test".to_string()),
            uri: Uri {
                scheme: "sip".to_string(),
                user: Some("user".to_string()),
                host: "test.com".to_string(),
                ..Default::default()
            },
            tag: Some("testtag".to_string()),
            ..Default::default()
        };
        assert_eq!(
            r#""Test" <sip:user@test.com>;tag=testtag"#,
            address.to_string()
        );

        let address = Address {
            uri: Uri {
                scheme: "sip".to_string(),
                user: Some("user".to_string()),
                host: "test.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!("<sip:user@test.com>", address.to_string());
    }

    #[test]
    fn uri_from_string() {
        let s = "sip:test@test.com:5080;transport=tcp;lr;user=phone";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(s, uri.to_string());
        assert_eq!(TransportType::Tcp, uri.transport);
        assert!(uri.lr);

        let s = "sip:test@test.com:5080";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!("sip".to_string(), uri.scheme);
        assert_eq!(Some("test".to_string()), uri.user);
        assert_eq!("test.com".to_string(), uri.host);
        assert_eq!(Some(5080), uri.port);
        assert_eq!(s, uri.to_string());

        let s = "sip:test@test.com";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(s, uri.to_string());

        let s = "sip:#617@test.com";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(Some("#617".to_string()), uri.user);
        assert_eq!(s, uri.to_string());

        let s = "sip:test.com";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(s, uri.to_string());

        let s = "sips:test@test.com;lr";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(s, uri.to_string());

        let s = "sip:test_test@test.com;lr";
        let uri = Uri::from_str(s).unwrap();
        assert_eq!(s, uri.to_string());
    }

    #[test]
    fn uri_to_string() {
        let uri = Uri {
            scheme: "sip".to_string(),
            user: Some("user".to_string()),
            host: "test.com".to_string(),
            ..Default::default()
        };
        assert_eq!(uri.to_string(), "sip:user@test.com");

        let uri = Uri {
            scheme: "sips".to_string(),
            user: Some("user".to_string()),
            host: "test.com".to_string(),
            ..Default::default()
        };
        assert_eq!(uri.to_string(), "sips:user@test.com");

        let uri = Uri {
            scheme: "sip".to_string(),
            host: "test.com".to_string(),
            ..Default::default()
        };
        assert_eq!(uri.to_string(), "sip:test.com");

        let uri = Uri {
            scheme: "sip".to_string(),
            user: Some("user".to_string()),
            host: "test.com".to_string(),
            port: Some(5080),
            ..Default::default()
        };
        assert_eq!("sip:user@test.com:5080", uri.to_string());

        let uri = Uri {
            scheme: "sip".to_string(),
            user: Some("user".to_string()),
            host: "test.com".to_string(),
            port: Some(5080),
            transport: TransportType::Tcp,
            ..Default::default()
        };
        assert_eq!("sip:user@test.com:5080;transport=tcp", uri.to_string());

        let uri = Uri {
            scheme: "sip".to_string(),
            user: Some("user".to_string()),
            host: "test.com".to_string(),
            port: Some(5080),
            lr: true,
            ..Default::default()
        };
        assert_eq!("sip:user@test.com:5080;lr", uri.to_string());
    }

    #[test]
    fn message_from_string() {
        let header_str = [
            "INVITE sip:1012@talk.yay.com SIP/2.0",
            "Via: SIP/2.0/UDP 127.0.0.1:5090;branch=z9hG4bKesldkfjsdklf",
            "Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bKesldkfjsdklf",
            "Route: <sip:10.0.0.1:5080;transport=tcp>",
            "Route: <sip:10.0.0.2>",
            "Max-Forwards: 70",
            "Contact: <sip:127.0.0.1:5080>",
            r#"To: "To Name" <sip:to@talk.yay.com>"#,
            r#"From: "Test Name" <sip:from@talk.yay.com>;tag=from_tag"#,
            "Call-ID: atestcallid",
            "CSeq: 1 INVITE",
            r#"Refer-To: <sip:4012@ansible.yay.com?Replaces=89373OTRjMmNiYWI5MjI5MDk1NjAxZDk2OTYxNWVkZTljZmU%3Bto-tag%3Dp2xkgrjucv%3Bfrom-tag%3Da4a72c4a>"#,
            "Content-Length: 746",
            "",
            "",
        ]
        .join("\r\n");

        let body_str = [
            "v=0",
            "o=FreeSWITCH 1482410438 1482410439 IN IP4 104.199.20.147",
            "s=FreeSWITCH",
            "c=IN IP4 104.199.20.147",
            "t=0 0",
            "m=Audio 19886 RTP/AVP 9 8 0 101 13",
            "a=rtpmap:9 G722/8000",
            "a=rtpmap:8 PCMA/8000",
            "a=rtpmap:0 PCMU/8000",
            "a=rtpmap:101 telephone-event/8000",
            "a=fmtp:101 0-16",
            "a=rtpmap:13 CN/8000",
            "a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:gnSzGnf4d9ORNarMnaVWRLzu+CqlkhAGdSWhfTaZ",
            "a=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:gnSzGnf4d9ORNarMnaVWRLzu+CqlkhAGdSWhfTaZ",
            "a=ptime:20",
            "a=sendrecv",
            "m=Video 61634 RTP/AVP 127 126",
            "a=rtpmap:127 H264/90000",
            "a=fmtp:127 profile-level-id=42800c;packetization-mode=0",
            "a=rtpmap:126 H264/90000",
            "a=fmtp:126 profile-level-id=42800c;packetization-mode=1",
            "a=rtcp-fb:* nack",
            "a=rtcp-fb:* nack pli",
            "a=rtcp-fb:* goog-remb",
            "a=sendrecv",
            "",
        ]
        .join("\r\n");

        let msg_str = [header_str, body_str.clone()].concat();
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!(msg_str, msg.to_string());
        assert_eq!(body_str, msg.body.unwrap());

        let msg_str = [
            "SIP/2.0 401 Unauthorized",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6017",
            "To: <sip:dongdong1@127.0.0.1:15060>",
            "From: <sip:dongdong1@127.0.0.1:15060>;tag=8299c663",
            "Call-ID: 89373ZDlkZjgzNjY2MDc4NTg3MzE5N2YxYzZjMzNiMTZjN2U",
            "CSeq: 1 REGISTER",
            "Content-Length: 0",
            "",
            "",
        ]
        .join("\r\n");
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!(msg_str, msg.to_string());

        let msg_str = [
            "REFER sip:1012@talk.yay.com SIP/2.0",
            "Via: SIP/2.0/UDP 127.0.0.1:5090;branch=z9hG4bKesldkfjsdklf",
            "Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bKesldkfjsdklf",
            "Route: <sip:10.0.0.1:5080;transport=tcp>",
            "Route: <sip:10.0.0.2>",
            "Max-Forwards: 70",
            "Contact: <sip:127.0.0.1:5080>",
            r#"To: "To Name" <sip:to@talk.yay.com>"#,
            r#"From: "Test Name" <sip:from@talk.yay.com>;tag=from_tag"#,
            "Call-ID: atestcallid",
            "CSeq: 1 REFER",
            r#"Refer-To: <tel:4012>"#,
            "Content-Length: 0",
            "",
            "",
        ]
        .join("\r\n");
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!(msg_str, msg.to_string());

        let msg_str = [
            "REFER sip:1012@talk.yay.com SIP/2.0",
            "Via: SIP/2.0/UDP 127.0.0.1:5090;branch=z9hG4bKesldkfjsdklf",
            "Via: SIP/2.0/TCP 127.0.0.1:5080;branch=z9hG4bKesldkfjsdklf",
            "Route: <sip:10.0.0.1:5080;transport=tcp>",
            "Route: <sip:10.0.0.2>",
            "Max-Forwards: 70",
            "Contact: <sip:127.0.0.1:5080>",
            r#"To: "To Name" <sip:to@talk.yay.com>"#,
            r#"From: "Test Name" <sip:from@talk.yay.com>;tag=from_tag"#,
            "Call-ID: atestcallid",
            "CSeq: 1 REFER",
            r#"Refer-To: <tel:1000>"#,
            "Content-Length: 0",
            "",
            "",
        ]
        .join("\r\n");
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!(msg_str, msg.to_string());
    }

    #[test]
    fn multiple_via_in_line() {
        let msg_str = [
            "SIP/2.0 401 Unauthorized",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6017,SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6018",
            "To: <sip:dongdong1@127.0.0.1:15060>",
            "From: <sip:dongdong1@127.0.0.1:15060>;tag=8299c663",
            "Call-ID: 89373ZDlkZjgzNjY2MDc4NTg3MzE5N2YxYzZjMzNiMTZjN2U",
            "CSeq: 1 REGISTER",
            "Content-Length: 0",
            "",
            "",
        ]
        .join("\r\n");
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!([
            "SIP/2.0 401 Unauthorized",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6017",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6018",
            "To: <sip:dongdong1@127.0.0.1:15060>",
            "From: <sip:dongdong1@127.0.0.1:15060>;tag=8299c663",
            "Call-ID: 89373ZDlkZjgzNjY2MDc4NTg3MzE5N2YxYzZjMzNiMTZjN2U",
            "CSeq: 1 REGISTER",
            "Content-Length: 0",
            "",
            ""].join("\r\n"), 
            msg.to_string(),
        );

        let msg_str = [
            "SIP/2.0 401 Unauthorized",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6017, SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6018",
            "To: <sip:dongdong1@127.0.0.1:15060>",
            "From: <sip:dongdong1@127.0.0.1:15060>;tag=8299c663",
            "Call-ID: 89373ZDlkZjgzNjY2MDc4NTg3MzE5N2YxYzZjMzNiMTZjN2U",
            "CSeq: 1 REGISTER",
            "Content-Length: 0",
            "",
            "",
        ]
        .join("\r\n");
        let msg = Message::from_str(&msg_str).unwrap();
        assert_eq!([
            "SIP/2.0 401 Unauthorized",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6017",
            "Via: SIP/2.0/UDP 127.0.0.1:64884;branch=z9hG4bK-524287-1---75d2624de55e6018",
            "To: <sip:dongdong1@127.0.0.1:15060>",
            "From: <sip:dongdong1@127.0.0.1:15060>;tag=8299c663",
            "Call-ID: 89373ZDlkZjgzNjY2MDc4NTg3MzE5N2YxYzZjMzNiMTZjN2U",
            "CSeq: 1 REGISTER",
            "Content-Length: 0",
            "",
            ""].join("\r\n"), 
            msg.to_string(),
        );
    }
}
