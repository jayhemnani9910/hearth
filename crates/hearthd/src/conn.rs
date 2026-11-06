//! Per-connection outbound queue. Producers never block: terminal data
//! that does not fit marks the subscription lost (the viewer must
//! re-attach), and control frames that do not fit close the connection.

use hearth_core::proto::{self, Frame};
use serde_json::json;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

pub const QUEUE_FRAMES: usize = 4096;
/// Bytes of queued frames allowed per connection before overflow.
pub const QUEUE_BYTES: usize = 8 << 20;

pub struct ConnOut {
    pub id: u64,
    tx: SyncSender<Frame>,
    closed: AtomicBool,
    lost: Mutex<Vec<u32>>,
    queued: AtomicUsize,
    pub close_reason: Mutex<Option<String>>,
}

fn frame_len(f: &Frame) -> usize {
    match f {
        Frame::Data { bytes, .. } | Frame::Snapshot { bytes, .. } | Frame::Input { bytes, .. } => bytes.len() + 16,
        Frame::Json(_) => 256,
    }
}

impl ConnOut {
    pub fn new(id: u64) -> (Arc<ConnOut>, Receiver<Frame>) {
        Self::with_capacity(id, QUEUE_FRAMES)
    }

    pub fn with_capacity(id: u64, cap: usize) -> (Arc<ConnOut>, Receiver<Frame>) {
        let (tx, rx) = mpsc::sync_channel(cap);
        (Arc::new(ConnOut { id, tx, closed: AtomicBool::new(false), lost: Mutex::new(Vec::new()), queued: AtomicUsize::new(0), close_reason: Mutex::new(None) }), rx)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub fn close(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            *self.close_reason.lock().unwrap() = Some(reason.to_string());
            // Wake the writer so it notices.
            let _ = self.tx.try_send(Frame::Json(json!({"type": "close", "reason": reason})));
        }
    }

    /// Control/ordered frames. Overflow closes the connection.
    pub fn send_ordered(&self, frame: Frame) {
        if self.is_closed() {
            return;
        }
        let n = frame_len(&frame);
        // Snapshots may exceed the budget on their own; allow one at a time.
        if self.queued.load(Ordering::Relaxed) > QUEUE_BYTES {
            self.close("outbound queue overflow (slow client)");
            return;
        }
        match self.tx.try_send(frame) {
            Ok(()) => {
                self.queued.fetch_add(n, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => self.close("outbound queue overflow (slow client)"),
            Err(TrySendError::Disconnected(_)) => self.close("writer gone"),
        }
    }

    /// Terminal data for one subscription. Returns false when the
    /// subscription must be dropped (lost or connection closed).
    pub fn send_data(&self, sub: u32, frame: &Frame) -> bool {
        if self.is_closed() {
            return false;
        }
        let n = frame_len(frame);
        if self.queued.load(Ordering::Relaxed) + n > QUEUE_BYTES {
            self.lost.lock().unwrap().push(sub);
            return false;
        }
        let f = match frame {
            Frame::Data { offset, bytes, .. } => Frame::Data { sub, offset: *offset, bytes: bytes.clone() },
            other => other.clone(),
        };
        match self.tx.try_send(f) {
            Ok(()) => {
                self.queued.fetch_add(n, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_)) => {
                self.lost.lock().unwrap().push(sub);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Writer loop: drains the queue to the stream until closed.
    pub fn run_writer(self: Arc<Self>, rx: Receiver<Frame>, mut w: impl Write) {
        let mut buf = std::io::BufWriter::with_capacity(1 << 16, &mut w);
        loop {
            let Ok(frame) = rx.recv() else { break };
            let is_close = matches!(&frame, Frame::Json(v) if v["type"] == "close");
            self.queued.fetch_sub(frame_len(&frame).min(self.queued.load(Ordering::Relaxed)), Ordering::Relaxed);
            if buf.write_all(&proto::encode(&frame)).is_err() {
                self.close("write failed");
                break;
            }
            // Coalesce queued frames into one write.
            while let Ok(f) = rx.try_recv() {
                self.queued.fetch_sub(frame_len(&f).min(self.queued.load(Ordering::Relaxed)), Ordering::Relaxed);
                if buf.write_all(&proto::encode(&f)).is_err() {
                    break;
                }
            }
            let lost: Vec<u32> = std::mem::take(&mut *self.lost.lock().unwrap());
            for sub in lost {
                let ev = Frame::Json(json!({"type": "event", "event": "terminal.lost", "data": {"sub": sub, "reason": "viewer queue overflow; re-attach for a fresh snapshot"}}));
                let _ = buf.write_all(&proto::encode(&ev));
            }
            if buf.flush().is_err() {
                self.close("write failed");
                break;
            }
            if is_close || (self.is_closed() && rx.try_recv().is_err()) {
                break;
            }
        }
    }
}
