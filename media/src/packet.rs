// RTP Packet Handling
// RTP packet handling for real-time media transmission
// Supports IPv4, UDP, and RTP packet construction and parsing

use byteorder::{BigEndian, ByteOrder};
use rand::RngCore;
use std::net::Ipv4Addr;

// Network protocol header lengths
const IPV4_HEADER_LEN: usize = 20;        // IPv4 header size
const UDP_HEADER_LEN: usize = 8;           // UDP header size
const UDP_PROTOCOL: u8 = 17;               // UDP protocol number
pub const RTP_VERSION: u8 = 2;              // RTP version 2
const RTP_HEADER_LEN: usize = 12;          // Basic RTP header size
pub const RTP_EXTENSIONS_HEADER_LEN: usize = 4;  // RTP extension header size
const RTCP_HEADER_LEN: usize = 4;          // RTCP header size
const RTCP_SSRC_LEN: usize = 4;            // RTCP SSRC field size

// RTP header field offsets
const MARKER_PT_OFFSET: usize = 1;        // Marker and payload type offset
const PACKET_TYPE_OFFSET: usize = 1;      // RTCP packet type offset
const LENGTH_OFFSET: usize = 2;            // Length field offset
const SEQUENCE_OFFSET: usize = 2;          // Sequence number offset
const TIMESTAMP_OFFSET: usize = SEQUENCE_OFFSET + 2;  // Timestamp offset
const SSRC_OFFSET_RTP: usize = TIMESTAMP_OFFSET + 4;  // SSRC offset in RTP
const SSRC_OFFSET_RTCP: usize = SEQUENCE_OFFSET + 2;  // SSRC offset in RTCP

// RTP header bit masks
const PT_MASK: u8 = 0x7f;                 // Payload type mask (7 bits)
const CC_MASK: u8 = 0x0f;                 // CSRC count mask (4 bits)
const EXTENSION_BIT: u8 = 0x10;           // Extension bit flag
const MARKER_BIT: u8 = 0x80;              // Marker bit flag

// IPv4 network layer packet
// This struct represents an IPv4 packet with full header support
// It provides methods for constructing, parsing, and manipulating IPv4 headers for network communication
#[derive(Clone)]
pub struct Ipv4Packet {
    inner: Vec<u8>,  // Raw packet data
}

// UDP transport layer packet
// This struct represents a UDP packet with header support
// It handles UDP header construction and provides methods for UDP-specific operations
pub struct UdpPacket {
    inner: Vec<u8>,  // Raw packet data
}

// RTP application layer packet
// This struct represents an RTP packet with full header support
// It provides methods for RTP header manipulation, payload handling and RTP-specific operations for real-time media transmission
#[derive(Clone)]
pub struct RtpPacket {
    inner: Vec<u8>,  // Raw packet data
}

// IPv4 helpers: build minimal IPv4+UDP container and fill header fields
impl Ipv4Packet {
    // Create a fresh IPv4 packet with default V4/TTL/UDP protocol headers
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

    // Set IPv4 version (normally 4) in the header's version field
    pub fn set_version(&mut self, val: u8) {
        self.inner[0] = (self.inner[0] & 15) | val << 4;
    }

    // Set IPv4 header length (IHL) given in bytes; stored as 32-bit words
    pub fn set_header_length(&mut self, val: u8) {
        self.inner[0] = (self.inner[0] & 240) | val / 4;
    }

    // Set total IPv4 length (header + payload) in bytes
    pub fn set_total_length(&mut self, val: u16) {
        let co = 2;
        self.inner[co + 0] = ((val & 65280) >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    // Set Time-To-Live for the IPv4 packet
    pub fn set_ttl(&mut self, val: u8) {
        let co = 8;
        self.inner[co + 0] = val;
    }

    // Set L4 protocol number (e.g., UDP = 17)
    pub fn set_protocol(&mut self, val: u8) {
        let co = 9;
        self.inner[co + 0] = val;
    }

    // Set IPv4 source address
    pub fn set_source_ip(&mut self, ip: Ipv4Addr) {
        let co = 12;
        let octets = ip.octets();
        self.inner[co + 0] = octets[0];
        self.inner[co + 1] = octets[1];
        self.inner[co + 2] = octets[2];
        self.inner[co + 3] = octets[3];
    }

    // Set IPv4 destination address
    pub fn set_destination_ip(&mut self, ip: Ipv4Addr) {
        let co = 16;
        let octets = ip.octets();
        self.inner[co + 0] = octets[0];
        self.inner[co + 1] = octets[1];
        self.inner[co + 2] = octets[2];
        self.inner[co + 3] = octets[3];
    }

    // Set UDP source port
    pub fn set_source_port(&mut self, val: u16) {
        let co = 20;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    // Set UDP destination port
    pub fn set_destination_port(&mut self, val: u16) {
        let co = 22;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    // Set UDP length (header + payload) in bytes
    pub fn set_udp_length(&mut self, val: u16) {
        let co = 24;
        self.inner[co + 0] = (val >> 8) as u8;
        self.inner[co + 1] = val as u8;
    }

    // Attach a UDP payload and compute the UDP checksum (with IPv4 pseudo-header)
    // Also updates UDP length and total IP length.
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

    // Return the full IPv4+UDP packet bytes
    pub fn get_slice(&self) -> &[u8] {
        &self.inner[..]
    }
}

// Folded 16-bit one's complement sum used by UDP checksum calculation
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

// RTP helpers: create/parse packets, manipulate header fields, and manage extensions
impl RtpPacket {
    // New RTP packet with v2 header and randomized SSRC/sequence/timestamp
    pub fn new() -> RtpPacket {
        let mut inner = vec![0; RTP_HEADER_LEN];
        inner[0] = 0x80;

        let mut p = RtpPacket { inner };
        p.new_ssrc();
        p.new_sequence();
        p.new_timestamp();
        p
    }

    // Quick sanity: first bit of the first byte must indicate RTP version field present
    // Minimal sanity check on an RTP-like buffer (version bit present)
    pub fn is_valid(buf: &[u8]) -> bool {
        buf.len() > 0 && buf[0] & 0x80 > 0
    }

    // True if the payload type falls into RTCP range (200..=206)
    // Heuristic: RTCP types are in the 200..=206 range (not general RTP PTs)
    pub fn is_rtcp(buf: &[u8]) -> bool {
        let marker: u8 = *buf.get(MARKER_PT_OFFSET).unwrap_or(&0);
        marker >= 200 && marker <= 206
    }

    // Wrap an existing RTP buffer without validation or copying
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { inner: buf }
    }

    // Read SSRC from this RTP packet
    pub fn ssrc(&self) -> u32 {
        Self::get_ssrc(&self.inner)
    }

    // Read SSRC from a raw RTP buffer
    pub fn get_ssrc(buf: &[u8]) -> u32 {
        let mut ssrc = (buf[8] as u32) << 24;
        ssrc += (buf[9] as u32) << 16;
        ssrc += (buf[10] as u32) << 8;
        ssrc += buf[11] as u32;
        ssrc
    }

    // Byte offset where RTP header extensions begin
    pub fn extension_offset(buffer: &[u8]) -> usize {
        (Self::get_csrc_count(buffer) as usize * 4 + RTP_HEADER_LEN) as usize
    }

    // Byte offset where RTP payload begins (after optional extensions)
    pub fn payload_offset(buffer: &[u8]) -> usize {
        (Self::get_csrc_count(buffer) as usize * 4 + RTP_HEADER_LEN) as usize
            + Self::get_extension_length(buffer)
    }

    // Randomize and set a new SSRC (32-bit)
    pub fn new_ssrc(&mut self) {
        let mut buffer = [0; 4];
        rand::thread_rng().fill_bytes(&mut buffer);
        let mut ssrc = buffer[0] as u32;
        ssrc |= (buffer[1] as u32) << 8;
        ssrc |= (buffer[2] as u32) << 16;
        ssrc |= (buffer[3] as u32) << 24;
        self.set_ssrc(ssrc);
    }

    // Write SSRC into a mutable RTP buffer
    pub fn set_packet_ssrc(buf: &mut [u8], ssrc: u32) {
        BigEndian::write_u32(&mut buf[SSRC_OFFSET_RTP..], ssrc);
    }

    // Write SSRC into this RTP packet
    pub fn set_ssrc(&mut self, ssrc: u32) {
        BigEndian::write_u32(&mut self.inner[SSRC_OFFSET_RTP..], ssrc);
    }

    // Randomize and set an initial sequence number
    pub fn new_sequence(&mut self) {
        let mut buffer = [0; 2];
        rand::thread_rng().fill_bytes(&mut buffer);
        let mut seq = buffer[0] as u16;
        seq |= (buffer[1] as u16) << 8;
        seq &= 0xEFFF;
        self.set_sequence(seq);
    }

    // Read sequence number from this RTP packet
    pub fn sequence(&self) -> u16 {
        BigEndian::read_u16(&self.inner[SEQUENCE_OFFSET..])
    }

    // Read sequence number from a raw RTP buffer
    pub fn get_sequence(buffer: &[u8]) -> u16 {
        BigEndian::read_u16(&buffer[SEQUENCE_OFFSET..])
    }

    // Write sequence number into a raw RTP buffer
    pub fn buffer_set_sequence(buffer: &mut [u8], seq: u16) {
        BigEndian::write_u16(&mut buffer[SEQUENCE_OFFSET..], seq);
    }

    // Write sequence number into this RTP packet.
    pub fn set_sequence(&mut self, seq: u16) {
        BigEndian::write_u16(&mut self.inner[SEQUENCE_OFFSET..], seq);
    }

    // Check if the marker bit is set on this RTP packet
    pub fn has_marker(&self) -> bool {
        (self.inner[MARKER_PT_OFFSET] >> 7 & 0x1) > 0
    }

    // Set or clear the RTP marker bit
    pub fn set_marker(&mut self, m: bool) {
        if m {
            self.inner[MARKER_PT_OFFSET] |= MARKER_BIT;
        } else {
            self.inner[MARKER_PT_OFFSET] &=
                self.inner[MARKER_PT_OFFSET] ^ MARKER_BIT;
        }
    }

    // Randomize and set an initial timestamp
    // Randomize and set an initial RTP timestamp (32-bit)
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

    // Read payload type (PT) from a raw RTP buffer
    pub fn get_payload_type(buffer: &[u8]) -> u8 {
        buffer[MARKER_PT_OFFSET] & PT_MASK
    }

    // Set payload type on owned RTP packet
    pub fn set_payload_type(&mut self, pt: u8) {
        self.inner[MARKER_PT_OFFSET] &= self.inner[MARKER_PT_OFFSET] ^ PT_MASK; // first: clear old type
        self.inner[MARKER_PT_OFFSET] |= pt & PT_MASK;
    }

    // Set payload type on a mutable buffer slice
    pub fn set_pt(buffer: &mut [u8], pt: u8) {
        buffer[MARKER_PT_OFFSET] &= buffer[MARKER_PT_OFFSET] ^ PT_MASK; // first: clear old type
        buffer[MARKER_PT_OFFSET] |= pt & PT_MASK;
    }

    // Replace payload in a mutable buffer, preserving header and extensions
    pub fn buffer_set_paylod(buffer: &mut Vec<u8>, payload: &[u8]) {
        let payload_offset = Self::payload_offset(buffer);
        buffer.truncate(payload_offset);
        buffer.extend_from_slice(payload);
    }

    // Replace payload on owned RTP packet, preserving header and extensions
    pub fn set_paylod(&mut self, payload: &[u8]) {
        let payload_offset = Self::payload_offset(&self.inner);
        self.inner.truncate(payload_offset);
        self.inner.extend_from_slice(payload);
    }

    // Return number of CSRC entries present in the header
    pub fn csrc_count(&self) -> u8 {
        Self::get_csrc_count(&self.inner)
    }

    // Return number of CSRC entries from a raw buffer
    pub fn get_csrc_count(buffer: &[u8]) -> u8 {
        buffer[0] & CC_MASK
    }

    // Return the byte length of the extension block (0 if no extension)
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

    // True if header extension bit is set.
    pub fn get_extension_bit(buffer: &[u8]) -> bool {
        (buffer[0] & EXTENSION_BIT) == EXTENSION_BIT
    }

    // Write/replace a raw extension block (0xBEDE format-length header + padded data)
    // Returns a new buffer with extension applied.
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

    // Return a slice of the raw extension data (excluding 0xBEDE + header size)
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

    // Read RTP timestamp from this packet
    pub fn timestamp(&self) -> u32 {
        Self::get_timestamp(&self.inner)
    }

    // Read RTP timestamp from a raw buffer
    pub fn get_timestamp(buffer: &[u8]) -> u32 {
        BigEndian::read_u32(&buffer[TIMESTAMP_OFFSET..])
    }

    // Borrow the payload slice from a raw RTP buffer
    pub fn get_payload(buffer: &[u8]) -> &[u8] {
        let payload_offset = Self::payload_offset(buffer);
        &buffer[payload_offset..]
    }

    // Check the marker bit on a raw RTP buffer
    pub fn get_marker(buffer: &[u8]) -> bool {
        (buffer[MARKER_PT_OFFSET] >> 7 & 0x1) > 0
    }

    // Mutably borrow the payload slice from a raw RTP buffer
    pub fn get_payload_mut(buffer: &mut [u8]) -> &mut [u8] {
        let payload_offset = Self::payload_offset(buffer);
        &mut buffer[payload_offset..]
    }

    // Borrow the payload slice from this RTP packet
    pub fn payload(&self) -> &[u8] {
        Self::get_payload(&self.inner)
    }

    // Mutably borrow the payload slice from this RTP packet
    pub fn payload_mut(&mut self) -> &mut [u8] {
        Self::get_payload_mut(&mut self.inner)
    }

    // Borrow all bytes of this RTP packet
    pub fn data(&self) -> &[u8] {
        &self.inner[..]
    }

    // Mutably borrow the underlying buffer for this RTP packet
    pub fn mut_data(&mut self) -> &mut Vec<u8> {
        &mut self.inner
    }

    // Increment sequence, wrapping at 16 bits, and return the new value
    pub fn next_seq(&mut self) -> u16 {
        let seq = Self::get_sequence(&self.inner).checked_add(1).unwrap_or(0);
        self.set_sequence(seq);
        seq
    }

    // Advance sequence and timestamp by ptime (samples per packet for the codec clock)
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

    // Only advance timestamp by ptime
    pub fn incr_timestamp(&mut self, ptime: u32) {
        self.set_timestamp(
            Self::get_timestamp(&self.inner)
                .checked_add(ptime)
                .unwrap_or(0),
        );
    }
}
