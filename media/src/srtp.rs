use crate::{packet::RtpPacket, rtcp::RtcpPacket};
use aes_ctr::{
    self,
    cipher::generic_array::GenericArray,
    cipher::{NewStreamCipher, SyncStreamCipher},
    Aes128Ctr,
};
use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ByteOrder, WriteBytesExt};
use hmac::{Hmac, Mac, NewMac};
use nebula_redis::REDIS;
use sha1::Sha1;
use std::{collections::HashMap, io::BufWriter, sync::Arc};
use subtle::ConstantTimeEq;

const MAX_ROCDISORDER: u16 = 100;
const MAX_SEQUENCE_NUMBER: u16 = 65535;
const MAX_SRTCP_INDEX: usize = 0x7FFFFFFF;

struct SSRCState {
    ssrc: u32,
    roc: u32,
    roc_has_processed: bool,
    last_seq: u16,
}

pub struct CryptoState {
    key: Arc<Vec<u8>>,
    auth_hash: Hmac<Sha1>,
    salt: Arc<Vec<u8>>,
    rtcp_key: Arc<Vec<u8>>,
    rtcp_auth_hash: Hmac<Sha1>,
    rtcp_salt: Arc<Vec<u8>>,
    tag_len: usize,
}

pub struct CryptoContext {
    uuid: String,
    pub state: Arc<CryptoState>,
    ssrc: HashMap<u32, SSRCState>,
    rtcp_index: usize,
}

impl CryptoContext {
    pub async fn new(
        uuid: String,
        master_key: Vec<u8>,
        master_salt: Vec<u8>,
        tag_len: usize,
    ) -> Result<Self> {
        nebula_task::spawn_task(move || -> Result<CryptoContext> {
            let master_key = &master_key;
            let master_salt = &master_salt;
            let key =
                Arc::new(Self::generate_key(16, master_key, master_salt, 0x00));
            let auth_tag = Self::generate_key(20, master_key, master_salt, 0x01);
            let salt =
                Arc::new(Self::generate_key(14, master_key, master_salt, 0x02));
            let auth_hash =
                Hmac::<Sha1>::new_varkey(&auth_tag).map_err(|e| anyhow!(e))?;

            let rtcp_key =
                Arc::new(Self::generate_key(16, master_key, master_salt, 0x03));
            let rtcp_auth_tag =
                Self::generate_key(20, master_key, master_salt, 0x04);
            let rtcp_salt =
                Arc::new(Self::generate_key(14, master_key, master_salt, 0x05));
            let rtcp_auth_hash =
                Hmac::<Sha1>::new_varkey(&rtcp_auth_tag).map_err(|e| anyhow!(e))?;
            Ok(Self {
                state: Arc::new(CryptoState {
                    key,
                    auth_hash,
                    salt,
                    rtcp_key,
                    rtcp_auth_hash,
                    rtcp_salt,
                    tag_len,
                }),
                uuid,
                ssrc: HashMap::new(),
                rtcp_index: 0,
            })
        })
        .await?
    }

    fn generate_key(
        len: usize,
        master_key: &[u8],
        master_salt: &[u8],
        label: u8,
    ) -> Vec<u8> {
        let nonce = Self::compute_nonce(master_salt, label);
        let key = GenericArray::from_slice(master_key);
        let nonce = GenericArray::from_slice(&nonce);
        let mut cipher = Aes128Ctr::new(key, nonce);
        let mut data = vec![0u8; len];
        cipher.apply_keystream(&mut data);
        data
    }

    fn compute_nonce(salt: &[u8], label: u8) -> Vec<u8> {
        let mut nonce = vec![0u8; 16];
        nonce[..7].copy_from_slice(&salt[..7]);
        let keyid = (label as i64) << 48;
        for i in 7..14 {
            nonce[i] = (0xff & (keyid >> (8 * (13 - i)))) as u8 ^ salt[i];
        }
        nonce
    }

    pub async fn update_roc(&mut self, packet: &[u8]) -> u32 {
        let seq = RtpPacket::get_sequence(packet);
        let ssrc = RtpPacket::get_ssrc(packet);

        let ssrc_state = self.ssrc.entry(ssrc).or_insert_with(|| SSRCState {
            ssrc,
            roc: 0,
            roc_has_processed: false,
            last_seq: 0,
        });
        ssrc_state.update_roc(&self.uuid, seq).await;
        ssrc_state.roc
    }

    pub async fn next_rtcp_index(&mut self) -> usize {
        self.rtcp_index += 1;
        if self.rtcp_index > MAX_SRTCP_INDEX {
            self.rtcp_index = 0;
        }
        self.rtcp_index
    }
}

impl SSRCState {
    pub async fn update_roc(&mut self, uuid: &str, seq: u16) {
        let get_key_field = || {
            (
                format!("nebula:media_stream:{uuid}"),
                format!("roc:{}", self.ssrc),
            )
        };

        if !self.roc_has_processed {
            let (key, field) = get_key_field();
            self.roc_has_processed = true;
            self.roc = REDIS.hget(&key, &field).await.unwrap_or(0);
        } else if seq == 0 {
            if self.last_seq > MAX_ROCDISORDER {
                let (key, field) = get_key_field();
                self.roc += 1;
                let _ = REDIS.hsetex(&key, &field, &self.roc.to_string()).await;
            }
        } else if self.last_seq < MAX_ROCDISORDER
            && seq > (MAX_SEQUENCE_NUMBER - MAX_ROCDISORDER)
        {
            let (key, field) = get_key_field();
            self.roc -= 1;
            let _ = REDIS.hsetex(&key, &field, &self.roc.to_string()).await;
        } else if seq < MAX_ROCDISORDER
            && self.last_seq > (MAX_SEQUENCE_NUMBER - MAX_ROCDISORDER)
        {
            let (key, field) = get_key_field();
            self.roc += 1;
            let _ = REDIS.hsetex(&key, &field, &self.roc.to_string()).await;
        }
        self.last_seq = seq;
    }
}

impl CryptoState {
    fn auth(&self, roc: Option<u32>, packet: &[u8]) -> Result<bool> {
        let auth_tag = &packet[packet.len() - self.tag_len..];
        let expected =
            self.generate_auth_tag(roc, &packet[..packet.len() - self.tag_len])?;
        Ok(auth_tag.ct_eq(expected.as_slice()).unwrap_u8() == 1)
    }

    pub fn encrypt_rtp(&self, roc: u32, packet: &mut RtpPacket) -> Result<()> {
        let seq = RtpPacket::get_sequence(packet.data());
        let ssrc = RtpPacket::get_ssrc(packet.data());
        let iv = self.generate_counter(&self.salt, ssrc, seq, roc);
        let key = GenericArray::from_slice(&self.key);
        let nonce = GenericArray::from_slice(&iv);
        let mut cipher = Aes128Ctr::new(key, nonce);
        let payload_offset = RtpPacket::payload_offset(packet.data());
        cipher.apply_keystream(&mut packet.mut_data()[payload_offset..]);
        let auth_tag = self.generate_auth_tag(Some(roc), packet.data())?;
        packet.mut_data().extend_from_slice(&auth_tag);
        Ok(())
    }

    pub fn decrypt_rtp(&self, roc: u32, packet: &[u8]) -> Result<Vec<u8>> {
        let seq = RtpPacket::get_sequence(packet);
        let ssrc = RtpPacket::get_ssrc(packet);
        if !self.auth(Some(roc), packet)? {
            Err(anyhow!("can't auth the srtp packet"))?;
        }
        let key = GenericArray::from_slice(&self.key);
        let iv = self.generate_counter(&self.salt, ssrc, seq, roc);
        let nonce = GenericArray::from_slice(&iv);
        let mut data = packet[..packet.len() - self.tag_len].to_vec();
        let mut cipher = Aes128Ctr::new(key, nonce);
        let payload_offset = RtpPacket::payload_offset(&data);
        cipher.apply_keystream(&mut data[payload_offset..]);
        Ok(data)
    }

    pub fn encrypt_rtcp(&self, packet: &[u8], index: usize) -> Result<Vec<u8>> {
        let ssrc = RtcpPacket::get_ssrc(packet);
        let iv = self.generate_counter(
            &self.rtcp_salt,
            ssrc,
            (index & 0xFFFF) as u16,
            (index >> 16) as u32,
        );
        let key = GenericArray::from_slice(&self.rtcp_key);
        let nonce = GenericArray::from_slice(&iv);
        let mut cipher = Aes128Ctr::new(key, nonce);
        let payload_offset = RtcpPacket::payload_offset();
        let tail_offset = packet.len();

        let mut packet = packet.to_vec();
        cipher.apply_keystream(&mut packet[payload_offset..]);
        packet.extend_from_slice(&[0, 0, 0, 0]);
        BigEndian::write_u32(
            &mut packet[tail_offset..tail_offset + 4],
            index as u32 | (1u32 << 31),
        );
        let mut auth_tag = self.generate_auth_tag(None, &packet)?;
        packet.append(&mut auth_tag);
        Ok(packet)
    }

    pub fn decrypt_rtcp(&self, packet: &[u8]) -> Result<Vec<u8>> {
        let tail_offset = packet.len() - self.tag_len - 4;
        if packet[tail_offset] >> 7 == 0 {
            return Ok(packet[..tail_offset].to_vec());
        }

        if !self.auth(None, packet)? {
            Err(anyhow!("can't auth the srtcp packet"))?;
        }

        let ssrc = RtcpPacket::get_ssrc(packet);
        let index = (BigEndian::read_u32(&packet[tail_offset..tail_offset + 4])
            & !(1 << 31)) as usize;
        let iv = self.generate_counter(
            &self.rtcp_salt,
            ssrc,
            (index & 0xFFFF) as u16,
            (index >> 16) as u32,
        );
        let key = GenericArray::from_slice(&self.rtcp_key);
        let nonce = GenericArray::from_slice(&iv);
        let mut data = packet[..tail_offset].to_vec();
        let mut cipher = Aes128Ctr::new(key, nonce);
        let payload_offset = RtcpPacket::payload_offset();
        cipher.apply_keystream(&mut data[payload_offset..]);
        Ok(data)
    }

    fn generate_counter(
        &self,
        salt: &[u8],
        ssrc: u32,
        seq: u16,
        roc: u32,
    ) -> Vec<u8> {
        let mut counter: Vec<u8> = vec![0; 16];
        counter[0..4].copy_from_slice(&salt[0..4]);

        BigEndian::write_u32(&mut counter[4..], ssrc);
        for i in 4..8 {
            counter[i] ^= salt[i];
        }

        let index = ((roc as u64) << 16) | (seq as u64);
        for i in 8..14 {
            counter[i] = (0xFF & ((index >> ((13 - i) * 8)) as u8)) ^ salt[i];
        }
        counter[14] = 0;
        counter[15] = 0;

        counter
    }

    fn generate_auth_tag(&self, roc: Option<u32>, packet: &[u8]) -> Result<Vec<u8>> {
        let mut auth_hash = if roc.is_some() {
            self.auth_hash.clone()
        } else {
            self.rtcp_auth_hash.clone()
        };
        auth_hash.reset();
        auth_hash.update(packet);

        if let Some(roc) = roc {
            let mut roc_buf: Vec<u8> = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(roc_buf.as_mut());
                writer.write_u32::<BigEndian>(roc)?;
            }
            auth_hash.update(&roc_buf);
        }

        let expected = auth_hash.finalize().into_bytes();
        Ok(expected.as_slice()[..self.tag_len].to_vec())
    }
}
