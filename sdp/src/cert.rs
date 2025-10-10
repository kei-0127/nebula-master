use std::collections::HashMap;

use anyhow::Result;
use nebula_redis::{DistributedMutex, REDIS};
use openssl::{
    asn1::Asn1Time,
    bn::{BigNum, MsbOption},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    rsa::Rsa,
    x509::{X509NameBuilder, X509},
};

pub struct Certificate {
    pub fingerprint: String,
    pub x509cert: X509,
    pub private_key: PKey<Private>,
}

pub async fn get_cert_fingerprint() -> Result<String> {
    let fingerprint = REDIS
        .hget("nebula:dtls:cert", "fingerprint")
        .await
        .unwrap_or("".to_string());
    if fingerprint == "" {
        let cert = get_cert().await?;
        Ok(cert.fingerprint)
    } else {
        Ok(fingerprint)
    }
}

async fn get_cert_from_cache() -> Option<Certificate> {
    let cert_map: HashMap<String, String> =
        REDIS.hgetall("nebula:dtls:cert").await.ok()?;
    let crt = cert_map.get("crt")?;
    let key = cert_map.get("key")?;
    let fingerprint = cert_map.get("fingerprint")?.to_string();
    let x509cert = X509::from_pem(crt.as_bytes()).ok()?;
    let private_key = PKey::private_key_from_pem(key.as_bytes()).ok()?;

    Some(Certificate {
        fingerprint,
        x509cert,
        private_key,
    })
}

pub async fn get_cert() -> Result<Certificate> {
    let mutex = DistributedMutex::new("nebula:dtls:cert:lock".to_string());
    mutex.lock().await;

    if let Some(cert) = get_cert_from_cache().await {
        return Ok(cert);
    }

    let cert = gen_cert()?;
    let crt = &cert.x509cert.to_pem()?;
    let key = &cert.private_key.private_key_to_pem_pkcs8()?;

    REDIS
        .hmset(
            "nebula:dtls:cert",
            vec![
                ("crt", std::str::from_utf8(crt)?),
                ("key", std::str::from_utf8(key)?),
                ("fingerprint", &cert.fingerprint),
            ],
        )
        .await?;
    REDIS.expire("nebula:dtls:cert", 60 * 60 * 24 * 360).await?;

    Ok(cert)
}

fn gen_cert() -> Result<Certificate> {
    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut x509_name = X509NameBuilder::new()?;
    x509_name.append_entry_by_nid(Nid::COMMONNAME, "nebula")?;
    let x509_name = x509_name.build();

    let mut big = BigNum::new()?;
    big.rand(128, MsbOption::MAYBE_ZERO, false)?;
    let serial_number = big.to_asn1_integer()?;

    let mut x509 = X509::builder()?;
    x509.set_subject_name(&x509_name)?;
    x509.set_issuer_name(&x509_name)?;
    x509.set_pubkey(&pkey)?;
    x509.set_serial_number(&serial_number)?;
    x509.set_version(0)?;
    x509.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
    x509.set_not_after(Asn1Time::days_from_now(365)?.as_ref())?;
    x509.sign(&pkey, MessageDigest::sha256())?;
    let x509 = x509.build();

    let fingerprint = x509
        .digest(MessageDigest::sha256())?
        .iter()
        .map(|n| format!("{:02X?}", n))
        .collect::<Vec<String>>()
        .join(":");

    Ok(Certificate {
        fingerprint,
        x509cert: x509,
        private_key: pkey,
    })
}
