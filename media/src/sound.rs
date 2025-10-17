//! # Audio Processing and Sound Management
//! 
//! This module provides comprehensive audio processing capabilities including
//! sound file handling, audio effects, mixing, recording, and playback.
//! It supports various audio formats and provides real-time audio processing.
//! 
//! ## Key Features
//! 
//! - **Sound File Support**: WAV, MP3, and other audio formats
//! - **Audio Effects**: Noise suppression, echo cancellation, audio mixing
//! - **Recording**: Real-time call recording with multiple formats
//! - **Playback**: Audio playback with volume control and effects
//! - **Text-to-Speech**: Integration with Google Text-to-Speech
//! - **Audio Generation**: Tone generation and audio synthesis
//! 
//! ## Audio Processing Pipeline
//! 
//! 1. **Input**: Audio samples from microphone or file
//! 2. **Processing**: Apply effects, mixing, resampling
//! 3. **Output**: Send to speakers or save to file
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_media::sound::{SessionSound, SoundFile};
//! 
//! // Create session sound
//! let sound = SessionSound::new(id, channel_id, sounds, random, repeat, block);
//! 
//! // Load sound file
//! let file = SoundFile::new("path/to/audio.wav");
//! ```

use crate::session::mix;

use super::resampler::Resampler;
use super::server::MEDIA_SERVICE;
use anyhow::{anyhow, Error, Result};
use base64;
use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use nebula_db::message::ChannelEvent;
use nebula_redis::REDIS;
use rand::seq::SliceRandom;
use rand::thread_rng;
use rand::Rng;
use std::str::from_utf8;
use std::sync::Arc;
use std::{f64::consts::PI, path::Path};
use texttospeechv1;
use thiserror::Error;
use tokio::fs;
use tokio::io::{AsyncReadExt, BufReader};
use tracing::error;
use tracing::info;
use async_channel::{self, Receiver};

/// Error types for sound file operations
#[derive(Debug, Error)]
pub enum SoundFileError {
    #[error("invalid file")]
    InvalidFile,
}

/// Session-level audio management
/// 
/// This struct manages audio playback for a specific session, including
/// sound file playback, random selection, and blocking behavior.
pub struct SessionSound {
    pub id: String,                    // Unique sound identifier
    pub channel_id: String,            // Associated channel ID
    pub block: bool,                   // Whether to block other audio
    pub random: bool,                   // Random sound selection
    pub receiver: Receiver<Vec<i16>>,  // Audio data receiver
}

/// Sound file representation
/// 
/// This struct represents an audio file and provides methods for
/// loading, processing, and playing audio content.
pub struct SoundFile {
    path: String,  // File path to audio content
}

impl SessionSound {
    /// Spawn a session sound producer.
    ///
    /// Selects files (optionally shuffled), repeats up to `repeat` times (or once if 0),
    /// emits PCM frames of `ptime` at `sample_rate`, and optionally blocks other audio.
    pub fn new(
        id: String,
        channel_id: String,
        mut sounds: Vec<String>,
        random: bool,
        repeat: u32,
        block: bool,
        sample_rate: u32,
        ptime: usize,
    ) -> SessionSound {
        let (sender, receiver) = async_channel::bounded(1000);
        let (step, repeat) = if repeat == 0 { (0, 1) } else { (1, repeat) };

        if random {
            sounds.shuffle(&mut thread_rng());
        }

        let skip = {
            let mut rng = thread_rng();
            rng.gen_range(0..3000)
        };

        tokio::spawn(async move {
            if sounds.len() == 0 {
                return;
            }
            let mut n = 0;
            let mut samples = 0;
            let mut played = false;
            while n < repeat {
                for s in sounds.iter() {
                    if let Some(file) = Self::get_file(s).await {
                        match file.read(ptime, sample_rate).await {
                            Ok(file_receiver) => {
                                while let Ok(pcm) = file_receiver.recv().await {
                                    played = true;
                                    samples += 1;
                                    if random && samples < skip {
                                        continue;
                                    }
                                    if sender.send(pcm).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => info!("file read error {}", e),
                        }
                    }
                }
                if !played {
                    return;
                }
                n += step;
            }
        });

        SessionSound {
            id,
            channel_id,
            block,
            random,
            receiver,
        }
    }

    /// Resolve a sound source string to a `SoundFile`.
    ///
    /// Supports schemes: `tts://`, `file://`, `vm://`, `tone://`, `redis://`,
    /// `record_file://`, or a database-backed name.
    async fn get_file(sound: &str) -> Option<SoundFile> {
        if sound.starts_with("tts://") {
            match SoundFile::new_tts(&sound[6..]).await {
                Ok(sound) => Some(sound),
                Err(_e) => None,
            }
        } else if sound.starts_with("file://") {
            Some(SoundFile::new_file(&sound[7..]))
        } else if sound.starts_with("vm://") {
            SoundFile::new_vm(&sound[5..]).await.ok()
        } else if sound.starts_with("tone://") {
            SoundFile::new_tone(&sound[7..]).await.ok()
        } else if sound.starts_with("redis://") {
            SoundFile::new_redis_file(&sound[8..]).await.ok()
        } else if sound.starts_with("record_file://") {
            Some(SoundFile::new_record_file(&sound[14..]))
        } else {
            match SoundFile::new(&sound).await {
                Ok(sound) => Some(sound),
                Err(_e) => None,
            }
        }
    }
}

impl Drop for SessionSound {
    /// On drop, publish a `SoundEnd` event for this session to the channel stream.
    fn drop(&mut self) {
        if self.channel_id != "" {
            let id = self.id.clone();
            let channel_id = self.channel_id.clone();
            tokio::spawn(async move {
                let result: Result<String> = REDIS
                    .xadd(
                        &format!("nebula:channel:{}:stream", &channel_id),
                        &ChannelEvent::SoundEnd.to_string(),
                        &id,
                    )
                    .await;
                if let Err(e) = result {
                    error!(
                        channel = channel_id,
                        "sound end error for channel {channel_id}: {e}"
                    );
                }
            });
        }
    }
}

impl SoundFile {
    /// Load or fetch a named sound and ensure there is a local WAV at `/var/lib/nebula/sounds`.
    ///
    /// If not present, downloads from object storage and normalizes via `sox`.
    pub async fn new(name: &str) -> Result<SoundFile, Error> {
        let sound = MEDIA_SERVICE
            .db
            .get_sound(name)
            .await
            .ok_or(anyhow!("no sound"))?;
        let file = match sound.time {
            Some(time) => {
                format!("{}.{}", sound.uuid, time.timestamp())
            }
            None => sound.uuid.clone(),
        };
        let path = format!("/var/lib/nebula/sounds/{}.wav", file);
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(SoundFile { path: path.clone() });
        }

        let tmp_file = format!("/tmp/{}.wav", file);
        let bytes = MEDIA_SERVICE
            .storage_client
            .get_object("aura-sounds", &format!("{}.wav", name))
            .await?;
        fs::write(&tmp_file, bytes).await?;
        if tokio::process::Command::new("sox")
            .arg("--vol=0.6")
            .arg(&tmp_file)
            .arg(&path)
            .output()
            .await
            .is_err()
        {
            fs::rename(&tmp_file, &path).await?;
        }

        Ok(SoundFile { path })
    }

    /// Build a voicemail sound path, fetching from object storage if missing.
    pub async fn new_vm(name: &str) -> Result<SoundFile, Error> {
        let path = format!("/var/lib/nebula/sounds/vm-{}.wav", name);
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(SoundFile { path: path.clone() });
        }

        let bytes = MEDIA_SERVICE
            .storage_client
            .get_object("aura-voicemails", &format!("{}.wav", name))
            .await?;
        fs::write(&path, bytes).await?;
        Ok(SoundFile { path })
    }

    /// Build a `SoundFile` pointing to a locally recorded WAV by UUID.
    pub fn new_record_file(uuid: &str) -> SoundFile {
        SoundFile {
            path: local_record_wav_file_path(uuid),
        }
    }

    /// Build a `SoundFile` from an absolute/relative filesystem path.
    pub fn new_file(name: &str) -> SoundFile {
        SoundFile {
            path: name.to_string(),
        }
    }

    /// Create a `SoundFile` from base64 content stored in Redis under `key`.
    pub async fn new_redis_file(key: &str) -> Result<SoundFile> {
        let path = format!("/var/lib/nebula/sounds/{}.wav", key);
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(SoundFile { path });
        }
        let content: String = REDIS.get(key).await?;
        let wav = base64::decode(&content)?;
        fs::write(&path, wav).await?;
        Ok(SoundFile { path })
    }

    /// Generate a tone WAV (two sine waves mixed with on/off cadence) and save it.
    pub async fn new_tone(tone: &str) -> Result<SoundFile> {
        let path = format!(
            "/var/lib/nebula/sounds/tone-{}.wav",
            nebula_utils::sha256(tone)
        );
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(SoundFile { path });
        }
        let parts: Vec<usize> = tone
            .split(",")
            .filter_map(|p| p.parse::<usize>().ok())
            .collect();
        if parts.len() != 4 {
            return Err(anyhow!("invalid tone format"));
        }
        let sample_rate = 32000;
        let pcm = nebula_task::spawn_task(move || {
            tone_pcm(sample_rate, parts[0], parts[1], parts[2], parts[3])
        })
        .await?;
        let wav = pcm_to_wav(sample_rate, pcm).await?;
        fs::write(&path, &wav).await?;
        Ok(SoundFile { path })
    }

    /// Synthesize speech via Google TTS into a 48kHz mono WAV and save it.
    ///
    /// Accepts optional `?voice=` query; defaults to `en-GB`.
    pub async fn new_tts(content: &str) -> Result<SoundFile, Error> {
        let path = format!(
            "/var/lib/nebula/sounds/tts-{}.wav",
            nebula_utils::sha256(content)
        );
        if fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(SoundFile { path: path.clone() });
        }

        let (input_text, name, language_code) = if let Some((content_str, voice_str)) = content.rsplit_once("?voice=") {
            (content_str, voice_str, &voice_str[..5.min(voice_str.len())])
        } else {
            (content, "", "en-GB")
        };

        let request = texttospeechv1::SynthesizeSpeechRequest {
            input: Some(texttospeechv1::SynthesisInput {
                input_source: Some(
                    texttospeechv1::synthesis_input::InputSource::Ssml(format!(
                        "<speak>{input_text}</speak>"
                    )),
                ),
            }),
            voice: Some(texttospeechv1::VoiceSelectionParams {
                name: name.to_string(),
                language_code: language_code.to_string(),
                ..Default::default()
            }),
            audio_config: Some(texttospeechv1::AudioConfig {
                audio_encoding: texttospeechv1::AudioEncoding::Linear16 as i32,
                sample_rate_hertz: 48000,
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut client = texttospeechv1::new_client().await?;
        let resp = client.synthesize_speech(request).await?.into_inner();
        fs::write(&path, resp.audio_content).await?;
        Ok(SoundFile { path })
    }

    /// Read a WAV file and stream PCM frames of length `ptime` at `dst_sample_rate`.
    ///
    /// Resamples if needed; returns an async channel receiver of `Vec<i16>` frames.
    pub async fn read(
        &self,
        ptime: usize,
        dst_sample_rate: u32,
    ) -> Result<Receiver<Vec<i16>>, Error> {
        let (sender, receiver) = async_channel::bounded(100);
        let mut reader = BufReader::new(fs::File::open(&self.path).await?);
        let mut buffer = [0; 44];
        reader.read_exact(&mut buffer).await?;

        if from_utf8(&buffer[..4])? != "RIFF" {
            return Err(SoundFileError::InvalidFile)?;
        }

        let src_sample_rate = LittleEndian::read_u32(&buffer[24..28]);
        let src_num_channels = LittleEndian::read_u16(&buffer[22..24]);
        let src_bits_per_sample = LittleEndian::read_u16(&buffer[34..36]);
        let bits = src_bits_per_sample / 8;
        let samples = src_sample_rate * ptime as u32 / 1000;
        let buf_len = (samples * src_num_channels as u32 * bits as u32) as usize;

        let src_pcm_len = (src_sample_rate * ptime as u32 / 1000) as usize;
        let dst_pcm_len = (dst_sample_rate * ptime as u32 / 1000) as usize;
        tokio::spawn(async move {
            let mut buffer = vec![0; buf_len];
            let resampler = Arc::new(parking_lot::Mutex::new(
                Resampler::new(src_sample_rate, dst_sample_rate)
                    .await
                    .unwrap(),
            ));
            loop {
                if reader.read_exact(&mut buffer).await.is_err() {
                    return;
                }
                let mut pcm = vec![0; src_pcm_len];
                for i in (0..buf_len).step_by(bits as usize) {
                    match bits {
                        2 => {
                            pcm[i / 2] =
                                LittleEndian::read_u16(&buffer[i..i + bits as usize])
                                    as i16
                        }
                        _ => (),
                    }
                }
                let pcm = if src_pcm_len == dst_pcm_len {
                    pcm
                } else {
                    let resampler = resampler.clone();
                    match nebula_task::spawn_task(move || {
                        resampler.lock().convert(&pcm)
                    })
                    .await
                    {
                        Ok(pcm) => pcm,
                        Err(_) => continue,
                    }
                };
                if sender.send(pcm).await.is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }
}

/// Generate a single-frequency sine wave of `duration` ms at `rate` Hz.
fn get_wave(rate: u32, duration: usize, freq: usize) -> Vec<i16> {
    let samples = (rate / 1000) as usize * duration;
    let mut phase = 0f64;
    let frequency_radian = freq as f64 * 2.0 * PI / rate as f64;
    let mut pcm = Vec::new();

    for _ in 0..samples {
        phase += frequency_radian;
        let value = phase.sin();
        pcm.push((value * 5000.0) as i16);
    }

    pcm
}

/// Create a dual-tone PCM buffer with `on`/`off` cadence and two frequencies.
fn tone_pcm(
    rate: u32,
    on: usize,
    off: usize,
    freq1: usize,
    freq2: usize,
) -> Vec<i16> {
    let pcm1 = get_wave(rate, on, freq1);
    let pcm2 = get_wave(rate, on, freq2);

    let mut pcm = Vec::new();
    for i in 0..pcm1.len() {
        pcm.push(mix(
            pcm1.get(i).map(|v| *v).unwrap_or(0),
            pcm2.get(i).map(|v| *v).unwrap_or(0),
        ));
    }

    for _ in 0..(rate / 1000) as usize * off {
        pcm.push(0);
    }
    pcm
}

/// Convert 16-bit mono PCM samples to a WAV byte vector (RIFF header + data).
pub async fn pcm_to_wav(sample_rate: u32, pcm: Vec<i16>) -> Result<Vec<u8>> {
    nebula_task::spawn_task(move || -> Result<Vec<u8>> {
        let pcm_len = pcm.len();
        let mut audio = vec![0; pcm_len * 2];
        for (i, v) in pcm.into_iter().enumerate() {
            LittleEndian::write_i16(&mut audio[2 * i..2 * i + 2], v);
        }

        let size = pcm_len as u32 * 1 * 16 / 8;

        let mut wav = Vec::new();
        wav.extend_from_slice("RIFF".as_bytes());
        wav.write_u32::<LittleEndian>(size + 36)?;
        wav.extend_from_slice("WAVE".as_bytes());

        wav.extend_from_slice("fmt ".as_bytes());
        wav.write_u32::<LittleEndian>(16)?;
        wav.write_u16::<LittleEndian>(1)?;
        wav.write_u16::<LittleEndian>(1)?;
        wav.write_u32::<LittleEndian>(sample_rate)?;
        wav.write_u32::<LittleEndian>(sample_rate * 16 / 8)?;
        wav.write_u16::<LittleEndian>(1 * 16 / 8)?;
        wav.write_u16::<LittleEndian>(16)?;

        wav.extend_from_slice("data".as_bytes());
        wav.write_u32::<LittleEndian>(size)?;
        wav.extend_from_slice(&audio);

        Ok(wav)
    })
    .await?
}

/// Local temporary WAV file path for a recording UUID.
pub fn local_record_wav_file_path(uuid: &str) -> String {
    format!("/tmp/record-file-{}.wav", uuid)
}

/// Local temporary MP3 file path for a recording UUID.
pub fn local_record_mp3_file_path(uuid: &str) -> String {
    format!("/tmp/record-file-{}.mp3", uuid)
}
