use async_channel::{self, Receiver, Sender};
use lazy_static::lazy_static;
use libc::c_int;
use nebula_redis::REDIS;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use spandsp_sys as ffi;
use std::{collections::HashMap, ffi::CString, io::Read, ptr, sync::Arc};
use tracing::{error, info};

const DEFAULT_FEC_ENTRIES: i32 = 3;

lazy_static! {
    static ref FAXES: Arc<Mutex<HashMap<i32, Arc<Mutex<FaxInfo>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

unsafe impl Send for Fax {}

struct FaxInfo {
    t30: *mut ffi::t30_state_s,
    uuid: String,
    tx_buf: HashMap<i32, Vec<u8>>,
    tx_seq: i32,
    outbound_sender: Option<Sender<Vec<u8>>>,
}

unsafe impl Send for FaxInfo {}

#[derive(Deserialize, Serialize, Debug)]
pub struct InitT38Params {
    pub version: usize,
    pub max_bit_rate: usize,
    pub max_buffer: usize,
    pub max_datagram: usize,
    pub fill_bit_removal: usize,
    pub transcoding_mmr: usize,
    pub transcoding_jbig: usize,
    pub rate_management: String,
}

pub struct Fax {
    uuid: String,
    rx_seq: u16,
    send: bool,
    data: i32,
    t38: Option<(*mut ffi::t38_core_state_t, *mut ffi::t38_terminal_state_t)>,

    file: CString,

    fax_state: *mut ffi::fax_state_t,
    t30: *mut ffi::t30_state_s,
}

impl Fax {
    pub fn new(uuid: String, send: bool) -> Arc<Mutex<Fax>> {
        let data = nebula_utils::rand_i32();
        let file = CString::new(format!("/tmp/{}.tiff", &uuid)).unwrap();
        let (fax_state, t30) = unsafe {
            let fax_state = ffi::fax_init(ptr::null_mut(), send);
            let t30 = ffi::fax_get_t30_state(fax_state);
            if send {
                ffi::t30_set_tx_file(t30, file.as_ptr(), -1, -1);
            } else {
                ffi::t30_set_rx_file(t30, file.as_ptr(), -1);
            }
            ffi::fax_set_transmit_on_idle(fax_state, 1);
            (fax_state, t30)
        };

        let mut fax = Fax {
            uuid: uuid.clone(),
            rx_seq: 0,
            file,
            fax_state,
            t30,
            t38: None,
            send,
            data,
        };
        fax.configure_t30();
        let fax = Arc::new(Mutex::new(fax));
        FAXES.lock().insert(
            data,
            Arc::new(Mutex::new(FaxInfo {
                t30,
                uuid,
                tx_buf: HashMap::new(),
                tx_seq: 0,
                outbound_sender: None,
            })),
        );
        fax
    }

    pub fn is_t38(&self) -> bool {
        self.t38.is_some()
    }

    pub fn init_t38(
        &mut self,
        t38_params: InitT38Params,
    ) -> Option<Receiver<Vec<u8>>> {
        if self.t38.is_some() {
            return None;
        }
        unsafe {
            // ffi::t30_terminate(self.t30);

            let t38_state = ffi::t38_terminal_init(
                ptr::null_mut(),
                self.send,
                Some(t38_tx_packet_handler),
                self.data as *mut std::os::raw::c_void,
            );
            let t38_core = ffi::t38_terminal_get_t38_core_state(t38_state);
            self.t30 = ffi::t38_terminal_get_t30_state(t38_state);
            if let Some(fax) = { FAXES.lock().get_mut(&self.data).cloned() } {
                fax.lock().t30 = self.t30;
            }
            self.configure_t30();

            ffi::t38_set_t38_version(t38_core, t38_params.version as i32);
            ffi::t38_set_max_buffer_size(t38_core, t38_params.max_buffer as i32);
            ffi::t38_set_fastest_image_data_rate(
                t38_core,
                t38_params.max_bit_rate as i32,
            );
            ffi::t38_set_fill_bit_removal(t38_core, t38_params.fill_bit_removal > 0);
            ffi::t38_set_mmr_transcoding(t38_core, t38_params.transcoding_mmr > 0);
            ffi::t38_set_jbig_transcoding(t38_core, t38_params.transcoding_jbig > 0);
            ffi::t38_set_max_datagram_size(t38_core, t38_params.max_datagram as i32);
            let mut method = 1;
            if t38_params.rate_management != "transferredTCF" {
                method = 2;
            }
            ffi::t38_set_data_rate_management_method(t38_core, method);

            self.t38 = Some((t38_core, t38_state));
        }

        let (outbound_sender, outbound_receiver) = async_channel::unbounded();

        {
            if let Some(info) = { FAXES.lock().get(&self.data).cloned() } {
                info.lock().outbound_sender = Some(outbound_sender);
            }
        }

        Some(outbound_receiver)
    }

    fn configure_t30(&mut self) {
        unsafe {
            if self.send {
                ffi::t30_set_tx_file(self.t30, self.file.as_ptr(), -1, -1);
            } else {
                ffi::t30_set_rx_file(self.t30, self.file.as_ptr(), -1);
            }
            ffi::t30_set_phase_b_handler(
                self.t30,
                Some(phase_b_handler),
                self.data as *mut std::os::raw::c_void,
            );
            ffi::t30_set_phase_d_handler(
                self.t30,
                Some(phase_d_handler),
                self.data as *mut std::os::raw::c_void,
            );
            ffi::t30_set_phase_e_handler(
                self.t30,
                Some(phase_e_handler),
                self.data as *mut std::os::raw::c_void,
            );
            ffi::t30_set_supported_image_sizes(
                self.t30,
                (ffi::T4_SUPPORT_LENGTH_US_LETTER
                    | ffi::T4_SUPPORT_LENGTH_US_LEGAL
                    | ffi::T4_SUPPORT_LENGTH_UNLIMITED
                    | ffi::T4_SUPPORT_WIDTH_215MM
                    | ffi::T4_SUPPORT_WIDTH_255MM
                    | ffi::T4_SUPPORT_WIDTH_303MM)
                    as std::os::raw::c_int,
            );
            ffi::t30_set_supported_bilevel_resolutions(
                self.t30,
                (ffi::T4_RESOLUTION_R8_STANDARD
                    | ffi::T4_RESOLUTION_R8_FINE
                    | ffi::T4_RESOLUTION_R8_SUPERFINE
                    | ffi::T4_RESOLUTION_R16_SUPERFINE
                    | ffi::T4_RESOLUTION_200_100
                    | ffi::T4_RESOLUTION_200_200
                    | ffi::T4_RESOLUTION_200_400
                    | ffi::T4_RESOLUTION_400_400)
                    as std::os::raw::c_int,
            );
            ffi::t30_set_supported_colour_resolutions(
                self.t30,
                (ffi::T4_RESOLUTION_100_100
                    | ffi::T4_RESOLUTION_200_200
                    | ffi::T4_RESOLUTION_300_300
                    | ffi::T4_RESOLUTION_400_400)
                    as std::os::raw::c_int,
            );
            ffi::t30_set_supported_compressions(
                self.t30,
                (ffi::t4_image_compression_t_T4_COMPRESSION_T4_1D
                    | ffi::t4_image_compression_t_T4_COMPRESSION_T4_2D
                    | ffi::t4_image_compression_t_T4_COMPRESSION_T6
                    | ffi::t4_image_compression_t_T4_COMPRESSION_T85
                    | ffi::t4_image_compression_t_T4_COMPRESSION_T85_L0
                    | ffi::t4_image_compression_t_T4_COMPRESSION_COLOUR
                    | ffi::t4_image_compression_t_T4_COMPRESSION_T42_T81
                    | ffi::t4_image_compression_t_T4_COMPRESSION_RESCALING
                    | ffi::t4_image_compression_t_T4_COMPRESSION_COLOUR_TO_BILEVEL
                    | ffi::t4_image_compression_t_T4_COMPRESSION_GRAY_TO_BILEVEL)
                    as std::os::raw::c_int,
            );
            ffi::t30_set_ecm_capability(self.t30, true);
            ffi::t30_set_supported_modems(
                self.t30,
                (ffi::T30_SUPPORT_V29
                    | ffi::T30_SUPPORT_V27TER
                    | ffi::T30_SUPPORT_V17) as std::os::raw::c_int,
            );
        }
    }

    pub fn process_data(
        &mut self,
        pcm: Option<&mut Vec<i16>>,
        pcm_len: usize,
    ) -> Vec<i16> {
        if let Some(pcm) = pcm {
            unsafe {
                ffi::fax_rx(self.fax_state, pcm.as_mut_ptr(), pcm.len() as c_int);
            }
        } else {
            unsafe {
                ffi::fax_rx_fillin(self.fax_state, pcm_len as c_int);
            }
        }
        let mut pcm = vec![0; pcm_len];
        let _ = unsafe {
            ffi::fax_tx(self.fax_state, pcm.as_mut_ptr(), pcm_len as c_int)
        };
        pcm
    }

    pub fn t38_send(&mut self) {
        if let Some((_, t38_state)) = self.t38.as_ref() {
            unsafe {
                ffi::t38_terminal_send_timeout(*t38_state, 160);
            }
        }
    }

    pub fn process_udptl(&mut self, buf: &[u8]) {
        let mut ptr = 0;
        if ptr + 2 > buf.len() {
            return;
        }

        let seq = ((buf[0] as u16) << 8) | buf[1] as u16;
        ptr += 2;

        if let Some((t38_core, _t38_state)) = self.t38.as_ref() {
            if let Some(msg) = decode_open_type(buf, &mut ptr) {
                if seq >= self.rx_seq {
                    unsafe {
                        ffi::t38_core_rx_ifp_packet(
                            *t38_core,
                            msg.as_ptr(),
                            msg.len() as c_int,
                            seq,
                        );
                    }
                }

                self.rx_seq = seq + 1;
            }
        }
    }
}

impl Drop for Fax {
    fn drop(&mut self) {
        info!("now dropping fax {}", self.uuid);
        unsafe {
            ffi::t30_terminate(self.t30);
            ffi::fax_release(self.fax_state);
        }

        if let Some((_, t38_state)) = self.t38.as_ref() {
            unsafe {
                ffi::t38_terminal_release(*t38_state);
            }
        }
    }
}

extern "C" fn t38_tx_packet_handler(
    _s: *mut ffi::t38_core_state_t,
    user_data: *mut ::std::os::raw::c_void,
    buf: *const u8,
    len: ::std::os::raw::c_int,
    count: ::std::os::raw::c_int,
) -> ::std::os::raw::c_int {
    let data = user_data as i32;
    let msg = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    let fax = { FAXES.lock().get(&data).cloned() };
    if let Some(fax) = fax {
        let mut fax = fax.lock();
        let seq = fax.tx_seq & 0xFFFF;
        let entry = seq & 15;
        fax.tx_buf.insert(entry, msg.to_vec());

        let mut data = Vec::new();
        data.push(((seq >> 8) & 0xFF) as u8);
        data.push((seq & 0xFF) as u8);
        data.extend_from_slice(&encode_open_type(msg).unwrap());
        data.push(0);

        let entries = if fax.tx_seq < DEFAULT_FEC_ENTRIES {
            fax.tx_seq
        } else {
            DEFAULT_FEC_ENTRIES
        };
        data.extend_from_slice(&encode_length(entries).unwrap());
        for m in 0..entries {
            let j = (entry - m - 1) & 15;
            if let Some(buf) = fax.tx_buf.get(&j) {
                data.extend_from_slice(&encode_open_type(buf).unwrap());
            }
        }
        fax.tx_seq += 1;
        if let Some(sender) = fax.outbound_sender.as_ref() {
            for _ in 0..count {
                let _ = sender.try_send(data.clone());
            }
        }
    }
    0
}

extern "C" fn phase_b_handler(
    user_data: *mut ::std::os::raw::c_void,
    result: ::std::os::raw::c_int,
) -> ::std::os::raw::c_int {
    let data = user_data as i32;
    0
}

extern "C" fn phase_d_handler(
    user_data: *mut ::std::os::raw::c_void,
    result: ::std::os::raw::c_int,
) -> ::std::os::raw::c_int {
    let data = user_data as i32;
    0
}

extern "C" fn phase_e_handler(
    user_data: *mut ::std::os::raw::c_void,
    completion_code: ::std::os::raw::c_int,
) {
    let data = user_data as i32;
    let fax = { FAXES.lock().remove(&data) };
    if let Some(fax) = fax {
        let fax = fax.lock();
        let mut stats = ffi::t30_stats_t {
            bit_rate: 0,
            error_correcting_mode: 0,
            pages_tx: 0,
            pages_rx: 0,
            pages_in_file: 0,
            x_resolution: 0,
            y_resolution: 0,
            width: 0,
            length: 0,
            image_size: 0,
            bad_rows: 0,
            longest_bad_row_run: 0,
            error_correcting_mode_retries: 0,
            current_status: 0,
            image_type: 0,
            image_x_resolution: 0,
            image_y_resolution: 0,
            image_width: 0,
            image_length: 0,
            type_: 0,
            compression: 0,
            rtp_events: 0,
            rtn_events: 0,
        };
        let mut status = "".to_string();
        unsafe {
            ffi::t30_get_transfer_statistics(fax.t30, &mut stats);
            if let Ok(s) = std::ffi::CStr::from_ptr(ffi::t30_completion_code_to_str(
                stats.current_status,
            ))
            .to_str()
            {
                status = s.to_string();
            }
        }

        info!(
            "phase e {} {} {} tiff exists {}",
            fax.uuid,
            completion_code,
            stats.pages_rx,
            std::path::PathBuf::from(format!("/tmp/{}.tiff", fax.uuid)).exists(),
        );

        match std::process::Command::new("tiff2pdf")
            .arg("-o")
            .arg(format!("/tmp/{}.pdf", fax.uuid))
            .arg(format!("/tmp/{}.tiff", fax.uuid))
            .output()
        {
            Ok(_result) => {
                info!("fax {} convert tiff to pdf successful", fax.uuid);
            }
            Err(e) => {
                error!("fax {} convert tiff to pdf error: {e}", fax.uuid);
            }
        }
        let mut got_pdf = false;
        if let Ok(buffer) = std::fs::read(format!("/tmp/{}.pdf", fax.uuid)) {
            let s = base64::encode(buffer);
            futures::executor::block_on(async {
                let key = format!("nebula:fax:{}", &fax.uuid);
                let _ = REDIS
                    .query::<String>("SETEX", &key, &[&key, "60", &s], false)
                    .await;
                got_pdf = true;
            });
            info!("fax {} upload pdf to redis", fax.uuid);
        } else {
            error!("fax {} doens't have pdf", fax.uuid);
        }
        let (completion_code, page) = if completion_code != 0 && got_pdf {
            // the fax didn't complete according to the code,
            // but we do received some pdf,
            // so we still mark it completed so that frontend can get the pdf so far
            (0, stats.pages_tx.max(1))
        } else {
            (completion_code, stats.pages_rx)
        };
        futures::executor::block_on(async {
            let stream = format!("nebula:channel:{}:stream", &fax.uuid);
            let value = format!(
                r#"
                {{
                    "code": {completion_code},
                    "status": "{status}",
                    "page": {page}
                }}
            "#
            );
            info!("fax {} send end info {value}", fax.uuid);

            let _ = REDIS
                .query::<String>(
                    "XADD",
                    &stream,
                    &[&stream, "*", "fax_end", &value],
                    false,
                )
                .await;
            let _ = REDIS
                .query::<bool>("EXPIRE", &stream, &[&stream, "60"], false)
                .await;
        });

        let _ = std::fs::remove_file(format!("/tmp/{}.tiff", fax.uuid));
        let _ = std::fs::remove_file(format!("/tmp/{}.pdf", fax.uuid));
    }
}

fn encode_length(length: i32) -> Option<Vec<u8>> {
    if length < 0x80 {
        return Some(vec![length as u8]);
    }

    if length < 0x4000 {
        return Some(vec![
            (((0x8000 | length) >> 8) & 0xFF) as u8,
            (length & 0xFF) as u8,
        ]);
    }
    None
}

fn encode_open_type(msg: &[u8]) -> Option<Vec<u8>> {
    let length_bytes = encode_length(msg.len() as i32)?;
    let mut data = length_bytes.to_vec();
    data.extend_from_slice(msg);
    Some(data)
}

fn decode_open_type(buf: &[u8], ptr: &mut usize) -> Option<Vec<u8>> {
    let length = decode_length(buf, ptr)?;
    let msg = buf[*ptr..*ptr + length].to_vec();
    *ptr += length;
    Some(msg)
}

fn decode_length(buf: &[u8], ptr: &mut usize) -> Option<usize> {
    if buf[*ptr] & 0x80 == 0 {
        let length = buf[*ptr] as usize;
        *ptr += 1;
        return Some(length);
    }
    if buf[*ptr] & 0x40 == 0 {
        let mut length = (buf[*ptr] as usize & 0x3F) << 8;
        *ptr += 1;
        length |= buf[*ptr] as usize;
        *ptr += 1;
        return Some(length);
    }

    None
}
