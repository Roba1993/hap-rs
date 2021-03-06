use std::{
    cmp::min,
    future::Future,
    io::{self, ErrorKind},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    task::{Context, Poll, Waker},
};

use aead::{generic_array::GenericArray, AeadInPlace, NewAead};
use byteorder::{ByteOrder, LittleEndian};
use bytes::{Buf, BytesMut};
use chacha20poly1305::{ChaCha20Poly1305, Nonce, Tag};
use futures::{
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    io::Error,
};
use log::{debug, error};
use ring::{digest, hkdf, hmac};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use uuid::Uuid;

use crate::Result;

#[derive(Debug)]
pub struct StreamWrapper {
    incoming_receiver: UnboundedReceiver<Vec<u8>>,
    outgoing_sender: UnboundedSender<Vec<u8>>,
    incoming_waker: Arc<Mutex<Option<Waker>>>,
    outgoing_waker: Arc<Mutex<Option<Waker>>>,
    incoming_buf: BytesMut,
}

impl StreamWrapper {
    pub fn new(
        incoming_receiver: UnboundedReceiver<Vec<u8>>,
        outgoing_sender: UnboundedSender<Vec<u8>>,
        incoming_waker: Arc<Mutex<Option<Waker>>>,
        outgoing_waker: Arc<Mutex<Option<Waker>>>,
    ) -> StreamWrapper {
        StreamWrapper {
            incoming_receiver,
            outgoing_sender,
            incoming_waker,
            outgoing_waker,
            incoming_buf: BytesMut::new(),
        }
    }

    fn poll_receiver(&mut self) -> Poll<usize> {
        debug!("polling incoming TCP stream receiver");

        match self.incoming_receiver.try_next() {
            Err(e) => {
                debug!("incoming TCP stream error: {}", e);
                Poll::Pending
            },
            Ok(Some(incoming)) => {
                self.incoming_buf.extend_from_slice(&incoming);
                let r_len = incoming.len();

                debug!("received {} Bytes on incoming TCP stream receiver", &r_len);

                Poll::Ready(r_len)
            },
            Ok(None) => {
                debug!("received 0 Bytes on incoming TCP stream receiver");
                Poll::Ready(0)
            },
        }
    }
}

impl AsyncRead for StreamWrapper {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<std::result::Result<usize, io::Error>> {
        let stream_wrapper = Pin::into_inner(self);

        match stream_wrapper.poll_receiver() {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_r_len) => {
                let r_len = min(buf.len(), stream_wrapper.incoming_buf.len());
                buf[..r_len].copy_from_slice(&stream_wrapper.incoming_buf[..r_len]);
                stream_wrapper.incoming_buf.advance(r_len);

                if let Some(waker) = stream_wrapper
                    .outgoing_waker
                    .lock()
                    .expect("accessing outgoing_waker")
                    .take()
                {
                    waker.wake()
                }
                if let Some(waker) = stream_wrapper
                    .incoming_waker
                    .lock()
                    .expect("accessing incoming_waker")
                    .take()
                {
                    waker.wake()
                }

                Poll::Ready(Ok(r_len))
            },
        }
    }
}

impl AsyncWrite for StreamWrapper {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context, buf: &[u8]) -> Poll<std::result::Result<usize, io::Error>> {
        let stream_wrapper = Pin::into_inner(self);

        debug!("writing {} Bytes to outgoing TCP stream sender", buf.len());

        stream_wrapper
            .outgoing_sender
            .unbounded_send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "couldn't write"))?;

        if let Some(waker) = stream_wrapper
            .outgoing_waker
            .lock()
            .expect("accessing outgoing_waker")
            .take()
        {
            waker.wake()
        }
        if let Some(waker) = stream_wrapper
            .incoming_waker
            .lock()
            .expect("accessing incoming_waker")
            .take()
        {
            waker.wake()
        }

        let w_len = buf.len();

        debug!("wrote {} Bytes to outgoing TCP stream sender", &w_len);

        Poll::Ready(Ok(w_len))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::result::Result<(), io::Error>> {
        // let stream_wrapper = Pin::into_inner(self);
        // Poll::Ready(Write::flush(stream_wrapper))
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::result::Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
pub struct Session {
    pub controller_id: Uuid,
    pub shared_secret: [u8; 32],
}

#[derive(Debug)]
pub struct EncryptedStream {
    stream: TcpStream,
    incoming_sender: UnboundedSender<Vec<u8>>,
    outgoing_receiver: UnboundedReceiver<Vec<u8>>,
    incoming_waker: Arc<Mutex<Option<Waker>>>,
    outgoing_waker: Arc<Mutex<Option<Waker>>>,
    session_receiver: oneshot::Receiver<Session>,
    pub controller_id: Arc<RwLock<Option<Uuid>>>,
    shared_secret: Option<[u8; 32]>,
    decrypt_count: u64,
    encrypt_count: u64,
    encrypted_buf: BytesMut,
    decrypted_buf: BytesMut,
    packet_len: usize,
    already_copied: usize,
    already_read: usize,
    decrypted_ready: bool,
    missing_data_for_decrypted_buf: bool,
    missing_data_for_encrypted_buf: bool,
}

impl EncryptedStream {
    pub fn new(
        stream: TcpStream,
    ) -> (
        EncryptedStream,
        UnboundedReceiver<Vec<u8>>,
        UnboundedSender<Vec<u8>>,
        oneshot::Sender<Session>,
        Arc<Mutex<Option<Waker>>>,
        Arc<Mutex<Option<Waker>>>,
    ) {
        let (sender, receiver) = oneshot::channel();
        let (incoming_sender, incoming_receiver) = mpsc::unbounded();
        let (outgoing_sender, outgoing_receiver) = mpsc::unbounded();
        let incoming_waker = Arc::new(Mutex::new(None));
        let outgoing_waker = Arc::new(Mutex::new(None));
        let mut encrypted_buf = BytesMut::new();
        encrypted_buf.resize(1042, 0);
        let mut decrypted_buf = BytesMut::new();
        decrypted_buf.resize(1024, 0);
        (
            EncryptedStream {
                stream,
                incoming_sender,
                outgoing_receiver,
                incoming_waker: incoming_waker.clone(),
                outgoing_waker: outgoing_waker.clone(),
                session_receiver: receiver,
                controller_id: Arc::new(RwLock::new(None)),
                shared_secret: None,
                decrypt_count: 0,
                encrypt_count: 0,
                encrypted_buf,
                decrypted_buf,
                packet_len: 0,
                already_copied: 0,
                already_read: 0,
                decrypted_ready: false,
                missing_data_for_decrypted_buf: false,
                missing_data_for_encrypted_buf: false,
            },
            incoming_receiver,
            outgoing_sender,
            sender,
            incoming_waker,
            outgoing_waker,
        )
    }

    fn read_decrypted(&mut self, buf: &mut [u8]) -> Poll<std::result::Result<usize, io::Error>> {
        debug!("reading from decrypted buffer");

        if self.decrypted_ready {
            let len = min(buf.len(), self.packet_len - 16);
            buf[..len].copy_from_slice(&self.decrypted_buf[..len]);
            self.already_copied = len;
            if self.already_copied == (self.packet_len - 16) {
                self.already_copied = 0;
                self.decrypted_ready = false;
            }

            return Poll::Ready(Ok(len));
        }

        Poll::Pending
    }

    fn read_encrypted(&mut self, buf: &mut [u8]) -> Poll<std::result::Result<usize, io::Error>> {
        debug!("reading from encrypted buffer");

        if self.missing_data_for_decrypted_buf {
            let decrypted = decrypt_chunk(
                &self.shared_secret.expect("missing shared secret"),
                &self.encrypted_buf[..2],
                &self.encrypted_buf[2..(self.packet_len - 14)],
                &self.encrypted_buf[(self.packet_len - 14)..(self.packet_len + 2)],
                &mut self.decrypt_count,
            )
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "decryption failed"))?;
            self.decrypted_buf[..decrypted.len()].copy_from_slice(&decrypted);
            self.missing_data_for_decrypted_buf = false;
            self.decrypted_ready = true;

            return self.read_decrypted(buf);
        }

        Poll::Pending
    }

    fn read_stream(&mut self, cx: &mut Context, buf: &mut [u8]) -> Poll<std::result::Result<usize, io::Error>> {
        debug!("reading from TCP stream");

        if self.missing_data_for_encrypted_buf {
            let r_len = AsyncRead::poll_read(
                Pin::new(&mut self.stream),
                cx,
                &mut self.encrypted_buf[self.already_read..],
            )?;

            match r_len {
                Poll::Pending => Poll::Pending,
                Poll::Ready(r_len) => {
                    if self.already_read + r_len == self.packet_len {
                        self.already_read = 0;
                        self.missing_data_for_encrypted_buf = false;
                        self.missing_data_for_decrypted_buf = true;

                        return self.read_encrypted(buf);
                    }

                    Poll::Pending
                },
            }
        } else {
            let r_len = AsyncRead::poll_read(
                Pin::new(&mut self.stream),
                cx,
                &mut self.encrypted_buf[self.already_read..2],
            )?;

            match r_len {
                Poll::Pending => Poll::Pending,
                Poll::Ready(r_len) => {
                    self.already_read += r_len;

                    if self.already_read == 2 {
                        self.packet_len = LittleEndian::read_u16(&self.encrypted_buf) as usize + 16;
                        self.missing_data_for_encrypted_buf = true;

                        let r_len = AsyncRead::poll_read(
                            Pin::new(&mut self.stream),
                            cx,
                            &mut self.encrypted_buf[self.already_read..],
                        )?;

                        match r_len {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(r_len) =>
                                if r_len == self.packet_len {
                                    self.already_read = 0;
                                    self.missing_data_for_encrypted_buf = false;
                                    self.missing_data_for_decrypted_buf = true;

                                    self.read_encrypted(buf)
                                } else {
                                    self.already_read += r_len;

                                    Poll::Pending
                                },
                        }
                    } else {
                        Poll::Pending
                    }
                },
            }
        }
    }

    fn poll_outgoing(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::result::Result<(), io::Error>> {
        let encrypted_stream = Pin::into_inner(self);
        loop {
            match encrypted_stream.outgoing_receiver.try_next() {
                Err(_) => {
                    *encrypted_stream.outgoing_waker.lock().expect("setting outgoing_waker") = Some(cx.waker().clone());
                    return Poll::Pending;
                },
                Ok(Some(data)) => {
                    debug!("writing {} Bytes to outgoing TCP stream", data.len());

                    match AsyncWrite::poll_write(Pin::new(encrypted_stream), cx, &data) {
                        Poll::Pending => {},
                        Poll::Ready(Err(e)) => {
                            error!("error writing to outgoing stream: {}", e);
                            return Poll::Ready(Err(e));
                        },
                        Poll::Ready(Ok(w_len)) => {
                            debug!("wrote {} Bytes to outgoing TCP stream", w_len);
                        },
                    };
                },
                Ok(None) => {
                    debug!("outgoing TCP stream ended");

                    return Poll::Ready(Ok(()));
                },
            }
        }
    }

    fn poll_incoming(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::result::Result<(), io::Error>> {
        let encrypted_stream = Pin::into_inner(self);
        let mut data = [0; 1536];
        loop {
            match AsyncRead::poll_read(Pin::new(encrypted_stream), cx, &mut data) {
                Poll::Pending => {
                    *encrypted_stream.incoming_waker.lock().expect("setting outgoing_waker") = Some(cx.waker().clone());
                    return Poll::Pending;
                },
                Poll::Ready(Err(e)) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        *encrypted_stream.incoming_waker.lock().expect("setting outgoing_waker") =
                            Some(cx.waker().clone());
                        return Poll::Pending;
                    },
                    _ => {
                        return Poll::Ready(Err(e));
                    },
                },
                Poll::Ready(Ok(r_len)) => {
                    if r_len == 0 {
                        return Poll::Ready(Ok(()));
                    }

                    encrypted_stream
                        .incoming_sender
                        .unbounded_send(data[..r_len].to_vec())
                        .map_err(|_| io::Error::new(io::ErrorKind::Other, "couldn't send incoming data"))?;
                },
            }
        }
    }
}

impl Future for EncryptedStream {
    type Output = std::result::Result<(), io::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let encrypted_stream = Pin::into_inner(self);
        EncryptedStream::poll_outgoing(Pin::new(encrypted_stream), cx)?;
        EncryptedStream::poll_incoming(Pin::new(encrypted_stream), cx)
    }
}

impl AsyncRead for EncryptedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<std::result::Result<usize, io::Error>> {
        let mut encrypted_stream = Pin::into_inner(self);

        if encrypted_stream.shared_secret.is_none() {
            match encrypted_stream.session_receiver.try_recv() {
                Ok(Some(session)) => {
                    *encrypted_stream.controller_id.write().expect("setting controller_id") =
                        Some(session.controller_id);
                    encrypted_stream.shared_secret = Some(session.shared_secret);
                },
                _ => {
                    return AsyncRead::poll_read(Pin::new(&mut encrypted_stream.stream), cx, buf);
                },
            }
        }

        match encrypted_stream.read_decrypted(buf) {
            Poll::Ready(Ok(size)) => Poll::Ready(Ok(size)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => match encrypted_stream.read_encrypted(buf) {
                Poll::Ready(Ok(size)) => Poll::Ready(Ok(size)),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => encrypted_stream.read_stream(cx, buf),
            },
        }
    }
}

impl AsyncWrite for EncryptedStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<std::result::Result<usize, Error>> {
        let encrypted_stream = Pin::into_inner(self);

        if let Some(shared_secret) = encrypted_stream.shared_secret {
            let mut write_buf = BytesMut::from(buf);

            while write_buf.len() > 1024 {
                let (aad, chunk, auth_tag) =
                    encrypt_chunk(&shared_secret, &write_buf[..1024], &mut encrypted_stream.encrypt_count)
                        .map_err(|_| io::Error::new(io::ErrorKind::Other, "encryption failed"))?;

                let data = [&aad[..], &chunk[..], &auth_tag[..]].concat();
                AsyncWrite::poll_write(Pin::new(&mut encrypted_stream.stream), cx, &data)?;

                write_buf.advance(1024);
            }

            let (aad, chunk, auth_tag) = encrypt_chunk(&shared_secret, &write_buf, &mut encrypted_stream.encrypt_count)
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "encryption failed"))?;

            let data = [&aad[..], &chunk[..], &auth_tag[..]].concat();
            AsyncWrite::poll_write(Pin::new(&mut encrypted_stream.stream), cx, &data)?;

            Poll::Ready(Ok(buf.len()))
        } else {
            AsyncWrite::poll_write(Pin::new(&mut encrypted_stream.stream), cx, buf)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::result::Result<(), Error>> {
        let encrypted_stream = Pin::into_inner(self);
        AsyncWrite::poll_flush(Pin::new(&mut encrypted_stream.stream), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<std::result::Result<(), Error>> {
        Poll::Ready(Ok(()))
    }
}

fn decrypt_chunk(
    shared_secret: &[u8; 32],
    aad: &[u8],
    data: &[u8],
    auth_tag: &[u8],
    count: &mut u64,
) -> Result<Vec<u8>> {
    let read_key = compute_read_key(shared_secret);
    let aead = ChaCha20Poly1305::new(GenericArray::from_slice(&read_key));

    let mut nonce = vec![0; 4];
    let mut suffix = vec![0; 8];
    LittleEndian::write_u64(&mut suffix, *count);
    nonce.extend(suffix);
    *count += 1;

    let mut buffer = Vec::new();
    buffer.extend_from_slice(data);
    aead.decrypt_in_place_detached(Nonce::from_slice(&nonce), aad, &mut buffer, Tag::from_slice(&auth_tag))?;

    Ok(buffer)
}

fn encrypt_chunk(shared_secret: &[u8; 32], data: &[u8], count: &mut u64) -> Result<([u8; 2], Vec<u8>, [u8; 16])> {
    let write_key = compute_write_key(shared_secret);
    let aead = ChaCha20Poly1305::new(GenericArray::from_slice(&write_key));

    let mut nonce = vec![0; 4];
    let mut suffix = vec![0; 8];
    LittleEndian::write_u64(&mut suffix, *count);
    nonce.extend(suffix);
    *count += 1;

    let mut aad = [0; 2];
    LittleEndian::write_u16(&mut aad, data.len() as u16);

    let mut buffer = Vec::new();
    buffer.extend_from_slice(data);
    let auth_tag = aead.encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut buffer)?;

    Ok((aad, buffer, auth_tag.into()))
}

fn compute_read_key(shared_secret: &[u8; 32]) -> [u8; 32] {
    compute_key(shared_secret, b"Control-Write-Encryption-Key")
}

fn compute_write_key(shared_secret: &[u8; 32]) -> [u8; 32] {
    compute_key(shared_secret, b"Control-Read-Encryption-Key")
}

fn compute_key(shared_secret: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut key = [0; 32];
    let salt = hmac::SigningKey::new(&digest::SHA512, b"Control-Salt");
    hkdf::extract_and_expand(&salt, shared_secret, &info, &mut key);
    key
}
