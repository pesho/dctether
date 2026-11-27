//! Channel multiplexing over the agent pipe, shared by both halves.
//!
//! Flow control is deliberately simple: every channel has a small bounded
//! queue, and the frame reader waits when a channel's queue is full. A slow
//! consumer therefore pauses the whole pipe (head-of-line blocking) instead
//! of growing memory without bound.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::proto::{self, DATA_CHUNK, Frame};
use crate::x11::ClientFilter;

/// Most channels open at once; further connections are refused.
pub const MAX_CHANNELS: usize = 256;
/// Frames queued per channel before the reader waits.
const CHANNEL_QUEUE: usize = 16;

pub type Out = mpsc::Sender<Frame>;

pub enum ChanMsg {
    Data(Vec<u8>),
    Eof,
}

/// Live channels and their inbound queues. Dropping a channel's sender
/// (removing it here) closes the channel for its pump.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<HashMap<u32, mpsc::Sender<ChanMsg>>>,
}

impl Registry {
    /// Register `id`. Returns `None` if it is in use or the limit is reached.
    pub fn insert(&self, id: u32) -> Option<mpsc::Receiver<ChanMsg>> {
        let mut map = self.inner.lock().unwrap();
        if map.contains_key(&id) || map.len() >= MAX_CHANNELS {
            return None;
        }
        let (tx, rx) = mpsc::channel(CHANNEL_QUEUE);
        map.insert(id, tx);
        Some(rx)
    }

    pub fn remove(&self, id: u32) {
        self.inner.lock().unwrap().remove(&id);
    }

    pub fn contains(&self, id: u32) -> bool {
        self.inner.lock().unwrap().contains_key(&id)
    }

    fn get(&self, id: u32) -> Option<mpsc::Sender<ChanMsg>> {
        self.inner.lock().unwrap().get(&id).cloned()
    }

    /// Deliver a Data/Eof/Close frame to its channel. Frames for unknown
    /// channels (already closed on this side) are dropped. Returns false for
    /// frame types that are not channel traffic.
    pub async fn dispatch(&self, frame: Frame) -> bool {
        match frame {
            Frame::Data { channel, data } => {
                if let Some(tx) = self.get(channel) {
                    let _ = tx.send(ChanMsg::Data(data)).await;
                }
            }
            Frame::Eof { channel } => {
                if let Some(tx) = self.get(channel) {
                    let _ = tx.send(ChanMsg::Eof).await;
                }
            }
            Frame::Close { channel } => self.remove(channel),
            Frame::Control(_) | Frame::Open { .. } => return false,
        }
        true
    }
}

/// Reads frames from the agent pipe.
pub struct FrameReader<R> {
    r: R,
    buf: BytesMut,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(r: R) -> Self {
        FrameReader {
            r,
            buf: BytesMut::with_capacity(64 * 1024),
        }
    }

    /// The next frame, or `None` at a clean end of stream.
    pub async fn next(&mut self) -> io::Result<Option<Frame>> {
        loop {
            if let Some(f) =
                proto::decode(&mut self.buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            {
                return Ok(Some(f));
            }
            if self.r.read_buf(&mut self.buf).await? == 0 {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed mid-frame"))
                };
            }
        }
    }
}

/// Encode frames from `rx` onto `w` until every sender is gone.
pub async fn write_frames<W: AsyncWrite + Unpin>(mut w: W, mut rx: mpsc::Receiver<Frame>) -> io::Result<()> {
    let mut buf = BytesMut::new();
    while let Some(f) = rx.recv().await {
        proto::encode(&f, &mut buf);
        // Batch whatever else is already queued.
        while buf.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(f) => proto::encode(&f, &mut buf),
                Err(_) => break,
            }
        }
        w.write_all(&buf).await?;
        w.flush().await?;
        buf.clear();
    }
    Ok(())
}

/// Relay one channel between a local stream and the pipe until both
/// directions have ended, or either side fails or closes it.
///
/// `filter`, if given, is applied to bytes arriving from the pipe before
/// they are written to `stream` (used for X11 on the host).
pub async fn pump<S>(
    id: u32,
    stream: S,
    mut inbox: mpsc::Receiver<ChanMsg>,
    out: Out,
    reg: Arc<Registry>,
    mut filter: Option<ClientFilter>,
) where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let out2 = out.clone();
    // Local → pipe. Returns true on a clean EOF.
    let mut reader = tokio::spawn(async move {
        let mut buf = vec![0u8; DATA_CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => return out2.send(Frame::Eof { channel: id }).await.is_ok(),
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if out2.send(Frame::Data { channel: id, data }).await.is_err() {
                        return false;
                    }
                }
                Err(_) => return false,
            }
        }
    });

    let mut local_done = false;
    let mut remote_done = false;
    let mut failed = false;
    let mut closed_by_peer = false;
    while !(local_done && remote_done) {
        tokio::select! {
            r = &mut reader, if !local_done => {
                local_done = true;
                if !r.unwrap_or(false) {
                    failed = true;
                    break;
                }
            }
            msg = inbox.recv(), if !remote_done => match msg {
                Some(ChanMsg::Data(d)) => {
                    let res = match &mut filter {
                        Some(f) => {
                            let mut o = Vec::with_capacity(d.len());
                            match f.feed(&d, &mut o) {
                                Ok(()) => wr.write_all(&o).await,
                                Err(e) => Err(io::Error::other(e)),
                            }
                        }
                        None => wr.write_all(&d).await,
                    };
                    if let Err(e) = res {
                        crate::debug!("channel {id}: {e}");
                        failed = true;
                        break;
                    }
                }
                Some(ChanMsg::Eof) => {
                    remote_done = true;
                    let _ = wr.shutdown().await;
                }
                None => {
                    closed_by_peer = true;
                    break;
                }
            }
        }
    }
    reader.abort();
    reg.remove(id);
    if failed && !closed_by_peer {
        let _ = out.send(Frame::Close { channel: id }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn pump_relays_both_ways_with_half_close() {
        let reg = Arc::new(Registry::default());
        let inbox = reg.insert(1).unwrap();
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (ours, mut theirs) = duplex(1024);
        let task = tokio::spawn(pump(1, ours, inbox, out_tx, reg.clone(), None));

        // Pipe → local.
        assert!(reg.dispatch(Frame::Data { channel: 1, data: b"ping".to_vec() }).await);
        reg.dispatch(Frame::Eof { channel: 1 }).await;
        let mut got = vec![];
        theirs.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"ping");

        // Local → pipe, after the other direction is already shut.
        theirs.write_all(b"pong").await.unwrap();
        theirs.shutdown().await.unwrap();
        assert_eq!(out_rx.recv().await, Some(Frame::Data { channel: 1, data: b"pong".to_vec() }));
        assert_eq!(out_rx.recv().await, Some(Frame::Eof { channel: 1 }));
        task.await.unwrap();
        assert!(!reg.contains(1));
    }

    #[tokio::test]
    async fn peer_close_ends_channel_without_echo() {
        let reg = Arc::new(Registry::default());
        let inbox = reg.insert(2).unwrap();
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (ours, _theirs) = duplex(1024);
        let task = tokio::spawn(pump(2, ours, inbox, out_tx, reg.clone(), None));
        reg.dispatch(Frame::Close { channel: 2 }).await;
        task.await.unwrap();
        // Dropping the pump drops its sender; nothing was sent.
        assert_eq!(out_rx.recv().await, None);
    }

    #[tokio::test]
    async fn filter_error_closes_channel() {
        let reg = Arc::new(Registry::default());
        let inbox = reg.insert(3).unwrap();
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (ours, _theirs) = duplex(1024);
        let filter = ClientFilter::new((b"N".to_vec(), vec![1]), (b"N".to_vec(), vec![2]));
        let task = tokio::spawn(pump(3, ours, inbox, out_tx, reg.clone(), Some(filter)));
        reg.dispatch(Frame::Data { channel: 3, data: b"x23456789012".to_vec() }).await;
        task.await.unwrap();
        assert_eq!(out_rx.recv().await, Some(Frame::Close { channel: 3 }));
    }

    #[test]
    fn registry_enforces_limit_and_uniqueness() {
        let reg = Registry::default();
        let _keep: Vec<_> = (0..MAX_CHANNELS as u32).map(|i| reg.insert(i + 1).unwrap()).collect();
        assert!(reg.insert(10_000).is_none());
        reg.remove(1);
        assert!(reg.insert(2).is_none());
        assert!(reg.insert(10_000).is_some());
    }
}
