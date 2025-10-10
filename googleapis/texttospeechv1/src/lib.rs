include!("./google.cloud.texttospeech.v1.rs");

pub use text_to_speech_client::*;

use anyhow::{Error, Result};
use tonic;
use tonic::metadata::MetadataValue;
use yup_oauth2;

const ENDPOINT: &str = "https://texttospeech.googleapis.com";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

pub async fn new_client(
) -> Result<TextToSpeechClient<tonic::transport::Channel>, Error> {
    let key =
        yup_oauth2::read_service_account_key("/etc/nebula/yayyay.json").await?;
    let authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(key)
        .build()
        .await?;
    let token = authenticator.token(&[SCOPE]).await?;

    let tls_config = tonic::transport::ClientTlsConfig::new();
    let channel = tonic::transport::Channel::from_static(ENDPOINT)
        .tls_config(tls_config)?
        .connect()
        .await?;
    let service = TextToSpeechClient::with_interceptor(
        channel,
        move |mut req: tonic::Request<()>| {
            let bearer_token = format!("Bearer {}", token.as_str());
            if let Ok(header_value) = MetadataValue::from_str(&bearer_token) {
                req.metadata_mut()
                    .insert("authorization", header_value.clone());
            }
            Ok(req)
        },
    );
    Ok(service)
}
