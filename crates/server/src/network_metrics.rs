//! Client-facing transport counters shared by TCP/TLS and QUIC. These counters
//! include protocol traffic, but exclude IP/TCP headers and outbound lookups.
#[path = "playback_presence.rs"]
mod playback_presence;
pub(crate) use playback_presence::PresenceError;

use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Default)]
pub struct NetworkMetrics {
    inner: Arc<Counters>,
}

#[derive(Default)]
struct Counters {
    sent_bytes: AtomicU64,
    received_bytes: AtomicU64,
    active_streams: AtomicU64,
    playback: Mutex<playback_presence::PlaybackPresence>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrafficTotals {
    pub sent_bytes: u64,
    pub received_bytes: u64,
}

fn add(counter: &AtomicU64, count: usize) {
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(count))
    });
}

impl NetworkMetrics {
    pub fn record_sent(&self, bytes: usize) {
        add(&self.inner.sent_bytes, bytes);
    }

    pub fn record_received(&self, bytes: usize) {
        add(&self.inner.received_bytes, bytes);
    }

    pub fn totals(&self) -> TrafficTotals {
        TrafficTotals {
            sent_bytes: self.inner.sent_bytes.load(Ordering::Relaxed),
            received_bytes: self.inner.received_bytes.load(Ordering::Relaxed),
        }
    }

    /// Open transfer bodies/ranges, retained for transport lifecycle checks.
    /// The performance API's active_streams field uses playing_devices instead.
    pub fn active_streams(&self) -> u64 {
        self.inner.active_streams.load(Ordering::Relaxed)
    }

    /// Playing clients remain counted while their audio ranges are buffered.
    pub fn playing_devices(&self) -> u64 {
        self.inner.playback.lock().playing_devices(Instant::now())
    }

    pub(crate) fn report_playback_presence(
        &self,
        user_id: &str,
        token: &str,
        client_id: &str,
        sequence: u64,
        playing: bool,
    ) -> Result<bool, PresenceError> {
        self.inner.playback.lock().report(
            user_id,
            &playback_token_digest(token),
            client_id,
            sequence,
            playing,
            Instant::now(),
        )
    }

    pub(crate) fn clear_playback_session(&self, token: &str) {
        self.inner
            .playback
            .lock()
            .clear_session(&playback_token_digest(token));
    }

    pub(crate) fn clear_playback_user_except(&self, user_id: &str, except_token: Option<&str>) {
        let digest = except_token.map(playback_token_digest);
        self.inner
            .playback
            .lock()
            .clear_user_except(user_id, digest.as_deref());
    }

    pub fn stream_started(&self) -> ActiveStreamGuard {
        add(&self.inner.active_streams, 1);
        ActiveStreamGuard(self.clone())
    }

    pub fn track_io<T>(&self, inner: T) -> TrackedIo<T> {
        TrackedIo {
            inner,
            metrics: self.clone(),
        }
    }
}

fn playback_token_digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Keep this guard with an audio response or stream until completion/cancel.
pub struct ActiveStreamGuard(NetworkMetrics);

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        let _ = self.0.inner.active_streams.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| Some(value.saturating_sub(1)),
        );
    }
}

/// Wrap the raw TCP socket before TLS so encryption/handshake bytes are counted
/// once, including the initial bytes buffered during protocol detection.
pub struct TrackedIo<T> {
    inner: T,
    metrics: NetworkMetrics,
}

impl<T: AsyncRead + Unpin> AsyncRead for TrackedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.metrics.record_received(buf.filled().len() - before);
        }
        result
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for TrackedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(bytes)) = result {
            this.metrics.record_sent(bytes);
        }
        result
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(bytes)) = result {
            this.metrics.record_sent(bytes);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn fragmented_reads_and_writes_count_successful_bytes_only() {
        let metrics = NetworkMetrics::default();
        let (socket, mut client) = tokio::io::duplex(64);
        let mut socket = metrics.track_io(socket);
        client.write_all(b"hello!").await.unwrap();
        let mut first = [0; 2];
        socket.read_exact(&mut first).await.unwrap();
        let mut rest = [0; 4];
        socket.read_exact(&mut rest).await.unwrap();
        assert_eq!(&first, b"he");
        assert_eq!(&rest, b"llo!");
        socket.write_all(b"reply").await.unwrap();
        socket.flush().await.unwrap();
        socket.shutdown().await.unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"reply");
        assert_eq!(
            metrics.totals(),
            TrafficTotals {
                sent_bytes: 5,
                received_bytes: 6
            }
        );
        // Dropping an open socket does not invent transferred bytes.
        drop(socket);
        assert_eq!(
            metrics.totals(),
            TrafficTotals {
                sent_bytes: 5,
                received_bytes: 6
            }
        );
    }

    struct PendingThenError {
        read_pending: bool,
        write_pending: bool,
    }

    impl AsyncRead for PendingThenError {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.read_pending {
                self.read_pending = false;
                Poll::Pending
            } else {
                Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()))
            }
        }
    }

    impl AsyncWrite for PendingThenError {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.write_pending {
                self.write_pending = false;
                Poll::Pending
            } else {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn pending_failed_and_cancelled_io_do_not_count_attempted_bytes() {
        let metrics = NetworkMetrics::default();
        let mut socket = metrics.track_io(PendingThenError {
            read_pending: true,
            write_pending: true,
        });
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut bytes = [0; 8];
        let mut buf = ReadBuf::new(&mut bytes);
        // An already-filled prefix is not new network traffic.
        buf.put_slice(b"x");
        assert!(Pin::new(&mut socket)
            .poll_read(&mut cx, &mut buf)
            .is_pending());
        assert!(matches!(
            Pin::new(&mut socket).poll_read(&mut cx, &mut buf),
            Poll::Ready(Err(_))
        ));
        assert!(Pin::new(&mut socket)
            .poll_write(&mut cx, b"hello")
            .is_pending());
        assert!(matches!(
            Pin::new(&mut socket).poll_write(&mut cx, b"hello"),
            Poll::Ready(Err(_))
        ));
        drop(socket);
        assert_eq!(metrics.totals(), TrafficTotals::default());
    }

    struct PartialWriter;
    impl AsyncWrite for PartialWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(bytes.len().min(2)))
        }
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(bytes
                .iter()
                .map(|part| part.len())
                .sum::<usize>()
                .min(3)))
        }
        fn is_write_vectored(&self) -> bool {
            true
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn partial_and_vectored_writes_count_the_returned_length_once() {
        let metrics = NetworkMetrics::default();
        let mut socket = metrics.track_io(PartialWriter);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(socket.is_write_vectored());
        assert!(matches!(
            Pin::new(&mut socket).poll_write(&mut cx, b"abcdef"),
            Poll::Ready(Ok(2))
        ));
        assert!(matches!(
            Pin::new(&mut socket)
                .poll_write_vectored(&mut cx, &[IoSlice::new(b"abc"), IoSlice::new(b"def")]),
            Poll::Ready(Ok(3))
        ));
        assert_eq!(metrics.totals().sent_bytes, 5);
    }

    #[test]
    fn stream_guards_share_live_count_and_release_on_drop() {
        let metrics = NetworkMetrics::default();
        let first = metrics.stream_started();
        let second = metrics.clone().stream_started();
        assert_eq!(metrics.active_streams(), 2);
        drop(first);
        assert_eq!(metrics.active_streams(), 1);
        drop(second);
        assert_eq!(metrics.active_streams(), 0);
        metrics
            .inner
            .sent_bytes
            .store(u64::MAX - 1, Ordering::Relaxed);
        metrics.record_sent(20);
        assert_eq!(metrics.totals().sent_bytes, u64::MAX);
    }
}
