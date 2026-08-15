//! One connection's outbound side: **a fair writer over per-stream queues**.
//!
//! [Operations §5](../../../docs/aperture-cli-design.md) asks for "a per-connection
//! single writer task that fairly interleaves ready streams (round-robin over
//! per-stream output queues)" and gives the reason: *without this, one chatty stream
//! starves the socket even when the executor has capacity*.
//!
//! # Why one queue would not do
//!
//! The obvious implementation is a single channel every stream pushes into, and it is
//! unfair in exactly the way that matters. A query returning a million rows fills the
//! channel; a second stream's four-frame answer queues behind all of them and waits
//! for the first query to finish. The socket was never the bottleneck — the *ordering*
//! was.
//!
//! So each stream has its own bounded queue and the writer takes **one frame from
//! each in turn**. A stream with a million frames gets one slot per rotation, exactly
//! like a stream with four.
//!
//! # Backpressure is what the bound is for
//!
//! A queue is bounded, and a producer that finds its own queue full waits. That is
//! `ops`-level flow control at the only place P0 has any: §5 defers per-stream
//! *windows* and says to start with "bounded per-stream queues + connection
//! backpressure", which is this. A stream that produces faster than the socket drains
//! blocks itself, and no other stream notices.

use std::collections::{BTreeMap, VecDeque};

use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Mutex, Notify},
};

use aperture_wire::{FrameKind, StreamId, encode_frame};

use crate::error::ServerError;

/// Frames one stream may have waiting before its producer is made to wait.
///
/// Small on purpose: the queue is a smoothing buffer, not a place to accumulate a
/// result. A large bound would let one stream build up a burst that the round-robin
/// then has to work through a frame at a time, which is the starvation this exists to
/// prevent, arriving more slowly.
pub const QUEUE_DEPTH: usize = 32;

/// The queues, and the rotation over them.
#[derive(Default)]
struct Queues {
    /// `BTreeMap`, so the rotation has a stable order and a stream that closes and
    /// reopens does not jump the queue by landing in a different hash bucket.
    ready: BTreeMap<u32, VecDeque<Vec<u8>>>,
    /// Where the last rotation stopped. The next take starts *after* this.
    cursor: u32,
    /// Set when the connection is finished, so the writer can drain and stop.
    closed: bool,
}

/// The outbound half of a connection.
///
/// Shared by every stream task and by the one writer task.
pub struct Outbound {
    queues: Mutex<Queues>,
    /// A frame arrived, or the connection closed.
    work: Notify,
    /// A slot was freed.
    room: Notify,
}

impl Default for Outbound {
    fn default() -> Self {
        Self::new()
    }
}

impl Outbound {
    #[must_use]
    pub fn new() -> Outbound {
        Outbound {
            queues: Mutex::new(Queues::default()),
            work: Notify::new(),
            room: Notify::new(),
        }
    }

    /// Queue a frame for `stream`, waiting if that stream's queue is full.
    ///
    /// Waits on **its own** queue only: a slow stream never blocks a fast one, which
    /// is the whole point of the queues being per-stream rather than shared.
    pub async fn send(
        &self,
        kind: FrameKind,
        stream: StreamId,
        payload: &[u8],
    ) -> Result<(), ServerError> {
        let mut frame = Vec::with_capacity(payload.len() + 16);
        encode_frame(&mut frame, kind, stream, payload)?;

        loop {
            {
                let mut queues = self.queues.lock().await;

                if queues.closed {
                    return Err(ServerError::Protocol(
                        "the connection closed while a stream was answering".to_owned(),
                    ));
                }

                let queue = queues.ready.entry(stream.0).or_default();

                if queue.len() < QUEUE_DEPTH {
                    queue.push_back(frame);
                    self.work.notify_one();
                    return Ok(());
                }
            }

            // Registered before the lock is retaken, so a slot freed in between is
            // not missed — `Notify` remembers one permit.
            self.room.notified().await;
        }
    }

    /// Stop the writer once everything queued has gone out.
    pub async fn close(&self) {
        self.queues.lock().await.closed = true;
        self.work.notify_one();
    }

    /// Take the next frame in rotation, or `None` when closed and drained.
    async fn next(&self) -> Option<Vec<u8>> {
        loop {
            {
                let mut queues = self.queues.lock().await;

                if let Some(frame) = queues.take_next() {
                    self.room.notify_one();
                    return Some(frame);
                }

                if queues.closed {
                    return None;
                }
            }

            self.work.notified().await;
        }
    }
}

impl Queues {
    /// One frame, from the stream after the cursor — round-robin.
    fn take_next(&mut self) -> Option<Vec<u8>> {
        // Split at the cursor and look after it first, so every stream is reached
        // before any stream is reached twice.
        let after = self.ready.range_mut((
            std::ops::Bound::Excluded(self.cursor),
            std::ops::Bound::Unbounded,
        ));

        let picked = after
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(id, _)| *id)
            .next()
            .or_else(|| {
                self.ready
                    .iter()
                    .filter(|(_, queue)| !queue.is_empty())
                    .map(|(id, _)| *id)
                    .next()
            })?;

        self.cursor = picked;

        let queue = self.ready.get_mut(&picked)?;
        let frame = queue.pop_front();

        // An empty queue is dropped rather than kept: a connection that opens
        // thousands of short-lived streams would otherwise grow a map entry apiece.
        if queue.is_empty() {
            self.ready.remove(&picked);
        }

        frame
    }
}

/// Drive the socket until the connection closes.
///
/// The **only** task that writes, which is what makes the interleaving a property of
/// the connection rather than of who happened to get there first.
///
/// # Errors
///
/// [`ServerError::Io`] if the socket fails.
pub async fn run<W: AsyncWrite + Unpin>(
    outbound: &Outbound,
    writer: &mut W,
) -> Result<(), ServerError> {
    while let Some(frame) = outbound.next().await {
        writer.write_all(&frame).await?;

        // Flushed when nothing else is waiting, rather than after every frame: a
        // batch of rows becomes one syscall, and a lone reply is not made to wait for
        // company that is not coming.
        if outbound.queues.lock().await.ready.is_empty() {
            writer.flush().await?;
        }
    }

    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind() -> FrameKind {
        FrameKind::DATA_ROW
    }

    /// **The property the whole module exists for**: a stream with a hundred frames
    /// waiting does not get a hundred turns before a stream with one gets its first.
    #[tokio::test]
    async fn a_chatty_stream_does_not_starve_a_quiet_one() {
        let outbound = Outbound::new();

        // Deliberately at the queue bound: more would block, which is the *other*
        // property and is tested below.
        for index in 0..QUEUE_DEPTH {
            outbound
                .send(kind(), StreamId(1), &[index as u8])
                .await
                .expect("it queues");
        }
        outbound
            .send(kind(), StreamId(2), b"quiet")
            .await
            .expect("it queues");

        // The first two frames out must be one from each stream.
        let first = outbound.next().await.expect("a frame");
        let second = outbound.next().await.expect("a frame");

        let stream_of = |frame: &[u8]| u32::from_le_bytes(frame[1..5].try_into().unwrap());

        assert_eq!(stream_of(&first), 1);
        assert_eq!(
            stream_of(&second),
            2,
            "the quiet stream must be reached before the chatty one is reached twice"
        );
    }

    /// The rotation keeps going round rather than favouring the lowest id.
    #[tokio::test]
    async fn the_rotation_visits_every_stream_in_turn() {
        let outbound = Outbound::new();

        for stream in 1..=3u32 {
            for _ in 0..3 {
                outbound
                    .send(kind(), StreamId(stream), &[stream as u8])
                    .await
                    .expect("it queues");
            }
        }

        let mut seen = vec![];
        for _ in 0..9 {
            let frame = outbound.next().await.expect("a frame");
            seen.push(u32::from_le_bytes(frame[1..5].try_into().unwrap()));
        }

        assert_eq!(seen, vec![1, 2, 3, 1, 2, 3, 1, 2, 3]);
    }

    /// A full queue makes its **own** producer wait and nobody else's.
    #[tokio::test]
    async fn a_full_queue_blocks_only_its_own_stream() {
        let outbound = std::sync::Arc::new(Outbound::new());

        for _ in 0..QUEUE_DEPTH {
            outbound
                .send(kind(), StreamId(1), b"x")
                .await
                .expect("it queues");
        }

        // One more on stream 1 must not complete yet.
        let blocked = {
            let outbound = std::sync::Arc::clone(&outbound);
            tokio::spawn(async move { outbound.send(kind(), StreamId(1), b"x").await })
        };

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !blocked.is_finished(),
            "a full queue should hold its producer"
        );

        // ...while another stream sails past it.
        outbound
            .send(kind(), StreamId(2), b"y")
            .await
            .expect("a different stream is unaffected");

        // Draining one frame frees the slot and releases the producer.
        outbound.next().await.expect("a frame");
        blocked.await.expect("the task joins").expect("it queues");
    }

    /// Closing drains what is queued and then stops, rather than dropping frames a
    /// stream believed it had sent.
    #[tokio::test]
    async fn closing_drains_what_is_already_queued() {
        let outbound = Outbound::new();

        outbound
            .send(kind(), StreamId(1), b"a")
            .await
            .expect("it queues");
        outbound
            .send(kind(), StreamId(1), b"b")
            .await
            .expect("it queues");
        outbound.close().await;

        assert!(outbound.next().await.is_some());
        assert!(outbound.next().await.is_some());
        assert!(outbound.next().await.is_none(), "and then it stops");

        assert!(
            outbound.send(kind(), StreamId(1), b"c").await.is_err(),
            "a closed connection refuses new frames rather than accepting and dropping them"
        );
    }
}
