use byteorder::{BigEndian, ByteOrder};
use rand::RngCore;
use std::net::Ipv4Addr;

const IPV4_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;
const UDP_PROTOCOL: u8 = 17;
pub const RTP_VERSION: u8 = 2;
const RTP_HEADER_LEN: usize = 12;
pub const RTP_EXTENSIONS_HEADER_LEN: usize = 4;
const RTCP_HEADER_LEN: usize = 4;
const RTCP_SSRC_LEN: usize = 4;

const MARKER_PT_OFFSET: usize = 1;
const PACKET_TYPE_OFFSET: usize = 1;
const LENGTH_OFFSET: usize = 2;
const SEQUENCE_OFFSET: usize = 2;
const TIMESTAMP_OFFSET: usize = SEQUENCE_OFFSET + 2;
const SSRC_OFFSET_RTP: usize = TIMESTAMP_OFFSET + 4;
const SSRC_OFFSET_RTCP: usize = SEQUENCE_OFFSET + 2;

const PT_MASK: u8 = 0x7f;
const CC_MASK: u8 = 0x0f;
const EXTENSION_BIT: u8 = 0x10;
const MARKER_BIT: u8 = 0x80;

#[derive(Clone)]
pub struct Ipv4Packet {
    inner: Vec<u8>,
}

pub struct UdpPacket {
    inner: Vec<u8>,
}

#[derive(Clone)]
pub struct RtpPacket {
    inner: Vec<u8>,
}

impl Ipv4Packet {
    pub fn new() -> Ipv4Packet {
        let mut inner = Vec::with_capacity(IPV4_HEADER_LEN + UDP_HEADER_LEN);
        for _ in 0..IPV4_HEADER_LEN + UDP_HEADER_LEN {
            inner.push(0);
        }

        let mut p = Ipv4Packet { inner };
        p.set_version(4);
        p.set_header_length(IPV4_HEADER_LEN as u8);
        p.set_ttl(64);
        p.set_protocol(17);
        p
    }

    pub fn set_version(&mut self, val: u8) {
        self.inner[0] = (self.inner[0] & 15) | val << 4;
    }

    pub fn set_header_length(&mut self, val: u8) {
        self.inner[0] = (self.inner[0] & 240) | val / 4;
    }

    pub fn set_total_length(&mut self, val: u16) {
        let co = 2;
        self.inner[co + 0] = ((val & 65280) >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    pub fn set_ttl(&mut self, val: u8) {
        let co = 8;
        self.inner[co + 0] = val;
    }

    pub fn set_protocol(&mut self, val: u8) {
        let co = 9;
        self.inner[co + 0] = val;
    }

    pub fn set_source_ip(&mut self, ip: Ipv4Addr) {
        let co = 12;
        let octets = ip.octets();
        self.inner[co + 0] = octets[0];
        self.inner[co + 1] = octets[1];
        self.inner[co + 2] = octets[2];
        self.inner[co + 3] = octets[3];
    }

    pub fn set_destination_ip(&mut self, ip: Ipv4Addr) {
        let co = 16;
        let octets = ip.octets();
        self.inner[co + 0] = octets[0];
        self.inner[co + 1] = octets[1];
        self.inner[co + 2] = octets[2];
        self.inner[co + 3] = octets[3];
    }

    pub fn set_source_port(&mut self, val: u16) {
        let co = 20;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    pub fn set_destination_port(&mut self, val: u16) {
        let co = 22;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    pub fn set_udp_length(&mut self, val: u16) {
        let co = 24;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    pub fn set_payload(&mut self, payload: &[u8]) {
        self.inner.truncate(IPV4_HEADER_LEN + UDP_HEADER_LEN);
        self.inner.extend_from_slice(payload);
        self.set_udp_length((payload.len() + UDP_HEADER_LEN) as u16);
        self.set_total_length(
            (payload.len() + IPV4_HEADER_LEN + UDP_HEADER_LEN) as u16,
        );
        let co = 26;
        self.inner[co + 0] = 0;
        self.inner[co + 1] = 0;

        let mut sum = csum(0, &self.inner[12..20]);
        sum = csum(sum, &[0, 17, 0, self.inner[25], self.inner[24]]);
        sum = csum(sum, &self.inner[20..]);
        sum ^= 0xffff;
        self.inner[co + 0] = (sum >> 8) as u8;
        self.inner[co + 1] = sum as u8;
    }

    pub fn get_slice(&self) -> &[u8] {
        &self.inner[..]
    }
}

fn csum(mut sum: u32, data: &[u8]) -> u32 {
    for i in 0..data.len() {
        if i & 1 == 0 {
            sum += (data[i] as u32) << 8;
        } else {
            sum += data[i] as u32;
        }
    }
    while sum > 0xffff {
        sum += sum >> 16;
        sum &= 0xffff;
    }
    sum
}

impl RtpPacket {
    pub fn new() -> RtpPacket {
        let mut inner = vec![0; RTP_HEADER_LEN];
        inner[0] = 0x80;

        let mut p = RtpPacket { inner };
        p.new_ssrc();
        p.new_sequence();
        p.new_timestamp();
        p
    }

    pub fn is_valid(buf: &[u8]) -> bool {
        buf.len() > 0 && buf[0] & 0x80 > 0
    }

    pub fn is_rtcp(buf: &[u8]) -> bool {
        let marker = *buf.get(MARKER_PT_OFFSET).unwrap_or(&0);
        marker >= 200 && marker <= 206
    }

    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { inner: buf }
    }

    pub fn ssrc(&self) -> u32 {
        Self::get_ssrc(&self.inner)
    }

    pub fn get_ssrc(buf: &[u8]) -> u32 {
        let mut ssrc = (buf[8] as u32) << 24;
        ssrc += (buf[9] as u32) << 16;
        ssrc += (buf[10] as u32) << 8;
        ssrc += buf[11] as u32;
        ssrc
    }

    pub fn extension_offset(buffer: &[u8]) -> usize {
        (Self::get_csrc_count(buffer) as usize * 4 + RTP_HEADER_LEN) as usize
    }

    pub fn payload_offset(buffer: &[u8]) -> usize {
        (Self::get_csrc_count(buffer) as usize * 4 + RTP_HEADER_LEN) as usize
            + Self::get_extension_length(buffer)
    }

    pub fn new_ssrc(&mut self) {
        let mut buffer = [0; 4];
        rand::thread_rng().fill_bytes(&mut buffer);
        let mut ssrc = buffer[0] as u32;
        ssrc |= (buffer[1] as u32) << 8;
        ssrc |= (buffer[2] as u32) << 16;
        ssrc |= (buffer[3] as u32) << 24;
        self.set_ssrc(ssrc);
    }

    pub fn set_packet_ssrc(buf: &mut [u8], ssrc: u32) {
        BigEndian::write_u32(&mut buf[SSRC_OFFSET_RTP..], ssrc);
    }

    pub fn set_ssrc(&mut self, ssrc: u32) {
        BigEndian::write_u32(&mut self.inner[SSRC_OFFSET_RTP..], ssrc);
    }

    pub fn new_sequence(&mut self) {
        let mut buffer = [0; 2];
        rand::thread_rng().fill_bytes(&mut buffer);
        let mut seq = buffer[0] as u16;
        seq |= (buffer[1] as u16) << 8;
        seq &= 0xEFFF;
        self.set_sequence(seq);
    }

    pub fn sequence(&self) -> u16 {
        BigEndian::read_u16(&self.inner[SEQUENCE_OFFSET..])
    }

    pub fn get_sequence(buffer: &[u8]) -> u16 {
        BigEndian::read_u16(&buffer[SEQUENCE_OFFSET..])
    }

    pub fn buffer_set_sequence(buffer: &mut [u8], seq: u16) {
        BigEndian::write_u16(&mut buffer[SEQUENCE_OFFSET..], seq);
    }

    pub fn set_sequence(&mut self, seq: u16) {
        BigEndian::write_u16(&mut self.inner[SEQUENCE_OFFSET..], seq);
    }

    pub fn has_marker(&self) -> bool {
        (self.inner[MARKER_PT_OFFSET] >> 7 & 0x1) > 0
    }

    pub fn set_marker(&mut self, m: bool) {
        if m {
            self.inner[MARKER_PT_OFFSET] |= MARKER_BIT;
        } else {
            self.inner[MARKER_PT_OFFSET] &=
                self.inner[MARKER_PT_OFFSET] ^ MARKER_BIT;
        }
    }

    pub fn new_timestamp(&mut self) {
        let mut buffer = [0; 4];
        rand::thread_rng().fill_bytes(&mut buffer);
        let mut ts = buffer[0] as u32;
        ts |= (buffer[1] as u32) << 8;
        ts |= (buffer[2] as u32) << 16;
        ts |= (buffer[3] as u32) << 24;
        ts &= 0xFFFFFFF;
        self.set_timestamp(ts);
    }

    pub fn set_timestamp(&mut self, ts: u32) {
        BigEndian::write_u32(&mut self.inner[TIMESTAMP_OFFSET..], ts);
    }

    pub fn get_payload_type(buffer: &[u8]) -> u8 {
        buffer[MARKER_PT_OFFSET] & PT_MASK
    }

    pub fn set_payload_type(&mut self, pt: u8) {
        self.inner[MARKER_PT_OFFSET] &= self.inner[MARKER_PT_OFFSET] ^ PT_MASK; // first: clear old type
        self.inner[MARKER_PT_OFFSET] |= pt & PT_MASK;
    }

    pub fn set_pt(buffer: &mut [u8], pt: u8) {
        buffer[MARKER_PT_OFFSET] &= buffer[MARKER_PT_OFFSET] ^ PT_MASK; // first: clear old type
        buffer[MARKER_PT_OFFSET] |= pt & PT_MASK;
    }

    pub fn buffer_set_paylod(buffer: &mut Vec<u8>, payload: &[u8]) {
        let payload_offset = Self::payload_offset(buffer);
        buffer.truncate(payload_offset);
        buffer.extend_from_slice(payload);
    }

    pub fn set_paylod(&mut self, payload: &[u8]) {
        let payload_offset = Self::payload_offset(&self.inner);
        self.inner.truncate(payload_offset);
        self.inner.extend_from_slice(payload);
    }

    pub fn csrc_count(&self) -> u8 {
        Self::get_csrc_count(&self.inner)
    }

    pub fn get_csrc_count(buffer: &[u8]) -> u8 {
        buffer[0] & CC_MASK
    }

    pub fn get_extension_length(buffer: &[u8]) -> usize {
        if !Self::get_extension_bit(buffer) {
            return 0;
        }

        let mut offset =
            ((Self::get_csrc_count(buffer) * 4) as usize + RTP_HEADER_LEN) as i16;
        offset += 2;
        let mut len = BigEndian::read_u16(&buffer[offset as usize..]) as usize + 1;
        len *= 4;
        len
    }

    pub fn extension_length(&self) -> usize {
        Self::get_extension_length(&self.inner)
    }

    pub fn get_extension_bit(buffer: &[u8]) -> bool {
        (buffer[0] & EXTENSION_BIT) == EXTENSION_BIT
    }

    pub fn set_extension(buffer: &[u8], data: &[u8]) -> Vec<u8> {
        let offset = (Self::get_csrc_count(buffer) * 4) as usize + RTP_HEADER_LEN;
        let extension_length = Self::get_extension_length(buffer);
        let mut new = buffer[..offset].to_vec();
        let new_extension_length = (data.len() as f64 / 4.0).ceil() as u16;
        new.extend_from_slice(&[0, 0, 0, 0]);
        BigEndian::write_u16(&mut new[offset + 2..], new_extension_length);
        let mut data = data.to_vec();
        if data.len() < new_extension_length as usize * 4 {
            for _i in 0..new_extension_length as usize * 4 - data.len() {
                data.insert(0, 0);
            }
        }
        new.extend_from_slice(&data);
        new.extend_from_slice(&buffer[offset + extension_length..]);
        new[0] |= 1 << 4;
        new
    }

    pub fn get_extension(buffer: &[u8]) -> Option<&[u8]> {
        let extension_length = Self::get_extension_length(buffer);
        if extension_length == 0 {
            return None;
        }
        let offset = (Self::get_csrc_count(buffer) * 4) as usize + RTP_HEADER_LEN;
        Some(&buffer[offset + 4..offset + extension_length])
    }

    // pub fn extension_bit(&self) -> bool {
    //     Self::get_extension_bit(&self.inner)
    // }

    pub fn timestamp(&self) -> u32 {
        Self::get_timestamp(&self.inner)
    }

    pub fn get_timestamp(buffer: &[u8]) -> u32 {
        BigEndian::read_u32(&buffer[TIMESTAMP_OFFSET..])
    }

    pub fn get_payload(buffer: &[u8]) -> &[u8] {
        let payload_offset = Self::payload_offset(buffer);
        &buffer[payload_offset..]
    }

    pub fn get_marker(buffer: &[u8]) -> bool {
        (buffer[MARKER_PT_OFFSET] >> 7 & 0x1) > 0
    }

    pub fn get_payload_mut(buffer: &mut [u8]) -> &mut [u8] {
        let payload_offset = Self::payload_offset(buffer);
        &mut buffer[payload_offset..]
    }

    pub fn payload(&self) -> &[u8] {
        Self::get_payload(&self.inner)
    }

    pub fn payload_mut(&mut self) -> &mut [u8] {
        Self::get_payload_mut(&mut self.inner)
    }

    pub fn data(&self) -> &[u8] {
        &self.inner[..]
    }

    pub fn mut_data(&mut self) -> &mut Vec<u8> {
        &mut self.inner
    }

    pub fn next_seq(&mut self) -> u16 {
        let seq = Self::get_sequence(&self.inner).checked_add(1).unwrap_or(0);
        self.set_sequence(seq);
        seq
    }

    pub fn next(&mut self, ptime: u32) {
        self.set_sequence(
            Self::get_sequence(&self.inner).checked_add(1).unwrap_or(0),
        );
        self.set_timestamp(
            Self::get_timestamp(&self.inner)
                .checked_add(ptime)
                .unwrap_or(0),
        );
    }

    pub fn incr_timestamp(&mut self, ptime: u32) {
        self.set_timestamp(
            Self::get_timestamp(&self.inner)
                .checked_add(ptime)
                .unwrap_or(0),
        );
    }
}
