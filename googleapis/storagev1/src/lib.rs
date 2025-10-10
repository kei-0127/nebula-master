// include!("./google.storage.v1.rs");

// pub use storage_client::*;

use anyhow::{anyhow, Error, Result};
use bytes::Bytes;
use reqwest;
use std::sync::Arc;
use tokio;
use tokio::fs;
use tokio::sync::Mutex;
use yup_oauth2::authenticator::{
    Authenticator, DefaultHyperClient, HyperClientBuilder,
};
use yup_oauth2::{self, AccessToken};

const ENDPOINT: &str = "https://storage.googleapis.com";
const SCOPE: &[&str] = &[
    // "https://www.googleapis.com/auth/cloud-platform",
    // "https://www.googleapis.com/auth/cloud-platform.read-only",
    "https://www.googleapis.com/auth/devstorage.full_control",
    // "https://www.googleapis.com/auth/devstorage.read_only",
    // "https://www.googleapis.com/auth/devstorage.read_write",
];

struct StroageState {
    token: Option<AccessToken>,
    authenticator:
        Option<Authenticator<<DefaultHyperClient as HyperClientBuilder>::Connector>>,
}

pub struct StorageClient {
    client: reqwest::Client,
    state: Arc<Mutex<StroageState>>,
}

impl StorageClient {
    pub fn new() -> StorageClient {
        let state = Arc::new(Mutex::new(StroageState {
            token: None,
            authenticator: None,
        }));
        StorageClient {
            state,
            client: reqwest::Client::new(),
        }
    }

    pub async fn get_object(
        &self,
        bucket: &str,
        name: &str,
    ) -> Result<Bytes, Error> {
        let bytes = self
            .client
            .get(&format!("{}/{}/{}", ENDPOINT, bucket, name))
            .header("Authorization", self.get_authorization().await?)
            .send()
            .await?
            .bytes()
            .await?;
        Ok(bytes)
    }

    pub async fn delete_object(
        &self,
        bucket: &str,
        name: &str,
    ) -> Result<(), Error> {
        let _resp = self
            .client
            .delete(&format!("{}/{}/{}", ENDPOINT, bucket, name))
            .header("Authorization", self.get_authorization().await?)
            .send()
            .await?;
        Ok(())
    }

    async fn get_authorization(&self) -> Result<String> {
        let mut state = self.state.lock().await;

        if state.authenticator.is_none() {
            let key =
                yup_oauth2::read_service_account_key("/etc/nebula/yayyay.json")
                    .await?;
            state.authenticator = Some(
                yup_oauth2::ServiceAccountAuthenticator::builder(key)
                    .build()
                    .await?,
            );
        }
        if state.token.is_none() {
            state.token =
                Some(state.authenticator.as_ref().unwrap().token(SCOPE).await?);
        }
        if state.token.as_ref().unwrap().is_expired() {
            state.token =
                Some(state.authenticator.as_ref().unwrap().token(SCOPE).await?);
        }
        let bearer_token =
            format!("Bearer {}", state.token.as_ref().unwrap().as_str());
        Ok(bearer_token)
    }

    pub async fn check_resumable_upload(&self, url: &str) -> Result<usize> {
        let resp = self
            .client
            .put(url)
            .header("Authorization", self.get_authorization().await?)
            .header("Content-Length", "0")
            .header("Content-Range", format!("bytes */*"))
            .send()
            .await?;
        if resp.status().as_u16() == 308 {
            if let Some(range) = resp.headers().get("Range") {
                return Ok(range
                    .to_str()?
                    .split("-")
                    .collect::<Vec<&str>>()
                    .get(1)
                    .unwrap_or(&"")
                    .parse::<usize>()?
                    + 1);
            } else {
                return Ok(0);
            }
        }
        Err(anyhow!("upload not 308"))
    }

    pub async fn finish_upload(
        &self,
        url: &str,
        content: &[u8],
        offset: usize,
    ) -> Result<()> {
        let _resp = self
            .client
            .put(url)
            .header("Authorization", self.get_authorization().await?)
            .header(
                "Content-Range",
                format!(
                    "bytes {}-{}/{}",
                    offset,
                    offset + content.len() - 1,
                    offset + content.len()
                ),
            )
            .body(content.to_vec())
            .send()
            .await?;
        Ok(())
    }

    pub async fn resume_upload(
        &self,
        url: &str,
        content: &[u8],
        offset: usize,
    ) -> Result<usize> {
        let resp = self
            .client
            .put(url)
            .header("Authorization", self.get_authorization().await?)
            .header(
                "Content-Range",
                format!("bytes {}-{}/*", offset, offset + content.len() - 1),
            )
            .body(content.to_vec())
            .send()
            .await?;
        if resp.status().as_u16() == 308 {
            let range = resp
                .headers()
                .get("Range")
                .ok_or(anyhow!("no Range in 308 response"))?
                .to_str()?;
            return Ok(range
                .split("-")
                .collect::<Vec<&str>>()
                .get(1)
                .unwrap_or(&"")
                .parse::<usize>()?);
        }

        let body = resp.text().await?;
        Err(anyhow!("upload file error {}", body))?
    }

    pub async fn upload_content(
        &self,
        bucket: &str,
        name: &str,
        content: Vec<u8>,
    ) -> Result<()> {
        let _resp = self
            .client
            .post(&format!(
                "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
                ENDPOINT, bucket, name
            ))
            .header("Authorization", self.get_authorization().await?)
            .body(content)
            .send()
            .await?;
        Ok(())
    }

    pub async fn upload(&self, bucket: &str, name: &str, path: &str) -> Result<()> {
        let content = fs::read(path).await?;
        self.upload_content(bucket, name, content).await
    }

    pub async fn new_resumable_upload(
        &self,
        bucket: &str,
        name: &str,
    ) -> Result<String> {
        let resp = self
            .client
            .post(&format!(
                "{}/upload/storage/v1/b/{}/o?uploadType=resumable&name={}",
                ENDPOINT, bucket, name
            ))
            .header("Authorization", self.get_authorization().await?)
            .header("Content-Length", 0)
            .send()
            .await?;
        if resp.status() != 200 {
            return Err(anyhow!(
                "new resumable upload failed: {}",
                resp.text().await.unwrap_or_default()
            ));
        }
        let location = resp
            .headers()
            .get("Location")
            .ok_or(anyhow!(
                "new resumable upload failed because of no location in response"
            ))?
            .to_str()?
            .to_string();
        Ok(location)
    }
}

// pub async fn new_client() -> Result<StorageClient<tonic::transport::Channel>, Error> {
//     let key = yup_oauth2::read_service_account_key("/etc/nebula/yayyay.json").await?;
//     let authenticator = yup_oauth2::ServiceAccountAuthenticator::builder(key)
//         .build()
//         .await?;
//     let token = ArcSwap::from(Arc::new(authenticator.token(SCOPE).await?));
//     let local_token = token.clone();

//     tokio::spawn(async move {
//         // let t = local_token.load();
//         // let mut ticker = time::interval(Duration::from_millis(1000));
//         // loop {
//         //     ticker.tick().await;
//         //     if let Ok(token) = authenticator.force_refreshed_token(SCOPE).await {
//         //         local_token.store(Arc::new(token));
//         //     }
//         // }
//     });

//     let tls_config = tonic::transport::ClientTlsConfig::new();
//     // .ca_certificate(Certificate::from_pem(certs.as_slice()))
//     // .domain_name("pubsub.googleapis.com");

//     let channel = tonic::transport::Channel::from_static(ENDPOINT)
//         .tls_config(tls_config)
//         .connect()
//         .await?;
//     let mut service =
//         StorageClient::with_interceptor(channel, move |mut req: tonic::Request<()>| {
//             let t = token.load();
//             println!("gcp token {}", t.as_str());
//             let bearer_token = format!("Bearer {}", t.as_str());
//             if let Ok(header_value) = MetadataValue::from_str(&bearer_token) {
//                 println!("add authorization");
//                 req.metadata_mut()
//                     .insert("authorization", header_value.clone());
//             }
//             Ok(req)
//         });
//     Ok(service)
// }
