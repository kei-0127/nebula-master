//! # DTLS Security Implementation
//! 
//! Datagram Transport Layer Security (DTLS) for secure WebRTC communication.
//! Provides encryption and authentication for media streams over UDP.

use crate::session::Certificate;
use anyhow::{anyhow, Result};
use openssl::{
    ssl::{
        self, MidHandshakeSslStream, Ssl, SslContext, SslMethod, SslOptions,
        SslStream, SslStreamBuilder, SslVerifyMode,
    },
    x509::X509StoreContextRef,
};
use std::future::Future;
use std::io::{self, Read, Write};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};

/// DTLS stream for secure WebRTC communication
/// Handles DTLS handshake, key exchange, and secure data transmission
#[derive(Debug)]
pub struct DtlsStream {
    sender: Sender<Vec<u8>>,      // Outgoing encrypted data
    receiver: Receiver<Vec<u8>>,  // Incoming encrypted data
    context: usize,               // DTLS context pointer
}

impl Read for DtlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ctx = unsafe { &mut *(self.context as *mut _) };
        match self.receiver.poll_recv(ctx) {
            Poll::Ready(r) => match r {
                Some(r) => {
                    if buf.len() == r.len() {
                        buf.copy_from_slice(&r);
                        Ok(buf.len())
                    } else if buf.len() > r.len() {
                        let (left, _) = buf.split_at_mut(r.len());
                        left.copy_from_slice(&r);
                        Ok(r.len())
                    } else {
                        buf.copy_from_slice(&r[..buf.len()]);
                        Ok(buf.len())
                    }
                }
                None => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            },
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }
}

impl Write for DtlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.sender.try_send(buf.to_vec()) {
            Ok(_) => Ok(buf.len()),
            Err(e) => match e {
                TrySendError::Closed(_) => {
                    Err(io::Error::from(io::ErrorKind::UnexpectedEof))
                }
                TrySendError::Full(_) => {
                    Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
            },
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DtlsStream {
    pub fn new() -> (DtlsStream, Sender<Vec<u8>>, Receiver<Vec<u8>>) {
        let (read_sender, read_receiver): (Sender<Vec<u8>>, Receiver<Vec<u8>>) =
            mpsc::channel(1000);
        let (write_sender, write_receiver): (Sender<Vec<u8>>, Receiver<Vec<u8>>) =
            mpsc::channel(1000);
        (
            DtlsStream {
                sender: write_sender,
                receiver: read_receiver,
                context: 0,
            },
            read_sender,
            write_receiver,
        )
    }
}

pub struct DtlsTransport {
    stream_builder: Option<SslStreamBuilder<DtlsStream>>,
    mid_stream: Option<MidHandshakeSslStream<DtlsStream>>,
    pub is_server: bool,
}

/// Verification callback – we accept anything here; DTLS peer auth is handled elsewhere.
fn verify(_a: bool, _ctx: &mut X509StoreContextRef) -> bool {
    true
}

impl DtlsTransport {
    /// Build a DTLS transport with the given certificate and role.
    /// Sets SRTP profile, read-ahead/MTU, and initializes accept/connect state.
    pub fn new(
        cert: Certificate,
        is_server: bool,
    ) -> Result<(DtlsTransport, Sender<Vec<u8>>, Receiver<Vec<u8>>)> {
        let mut builder = SslContext::builder(SslMethod::dtls())?;
        builder.set_read_ahead(true);
        // builder.set_verify(SslVerifyMode::NONE);
        builder.set_verify_callback(SslVerifyMode::NONE, verify);
        builder.set_private_key(&cert.private_key)?;
        builder.set_certificate(&cert.x509cert)?;
        builder.check_private_key()?;
        builder.set_tlsext_use_srtp("SRTP_AES128_CM_SHA1_80")?;
        builder.set_options(SslOptions::NO_QUERY_MTU);
        let ssl_context = builder.build();

        let mut ssl = Ssl::new(&ssl_context)?;
        ssl.set_mtu(1400)?;

        let (dtls_stream, sender, receiver) = DtlsStream::new();
        let mut stream_builder = SslStreamBuilder::new(ssl, dtls_stream);
        if is_server {
            stream_builder.set_accept_state();
        } else {
            stream_builder.set_connect_state();
        }
        Ok((
            DtlsTransport {
                stream_builder: Some(stream_builder),
                mid_stream: None,
                is_server,
            },
            sender,
            receiver,
        ))
    }

    /// After the handshake completes, export SRTP keying material (RFC 5764) for SRTP/SRTCP.
    pub async fn get_srtp_key(self) -> Result<[u8; 60]> {
        let mut buf = [0 as u8; 60];
        let ssl_stream = self.await?;
        ssl_stream.ssl().export_keying_material(
            &mut buf,
            "EXTRACTOR-dtls_srtp",
            None,
        )?;
        Ok(buf)
    }
}

impl Future for DtlsTransport {
    type Output = Result<SslStream<DtlsStream>>;

    /// Drive the DTLS handshake with OpenSSL, keeping mid-handshake state across polls.
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = if let Some(mut stream_builder) = self.stream_builder.take() {
            stream_builder.get_mut().context = ctx as *mut _ as usize;
            stream_builder.handshake()
        } else {
            let mut mid_stream =
                self.mid_stream.take().expect("no mid handshake stream");
            mid_stream.get_mut().context = ctx as *mut _ as usize;
            mid_stream.handshake()
        };
        match result {
            Ok(mut s) => {
                s.get_mut().context = 0;
                Poll::Ready(Ok(s))
            }
            Err(ssl::HandshakeError::WouldBlock(mut s)) => {
                s.get_mut().context = 0;
                self.mid_stream = Some(s);
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(anyhow!("handshake failed {}", e))),
        }
    }
}
