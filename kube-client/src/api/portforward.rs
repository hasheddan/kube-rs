use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use bytes::{Buf, Bytes};
use futures::{
    channel::{mpsc, oneshot},
    future, FutureExt, SinkExt, StreamExt,
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio_tungstenite::{tungstenite as ws, WebSocketStream};
use tokio_util::io::ReaderStream;

/// Errors from Portforwarder.
#[derive(Debug, Error)]
pub enum Error {
    /// Received invalid channel in WebSocket message.
    #[error("received invalid channel {0}")]
    InvalidChannel(usize),

    /// Received initial frame with invalid size. The initial frame must be 3 bytes, including the channel prefix.
    #[error("received initial frame with invalid size")]
    InvalidInitialFrameSize,

    /// Received initial frame with invalid port mapping.
    /// The port included in the initial frame did not match the port number associated with the channel.
    #[error("invalid port mapping in initial frame, got {actual}, expected {expected}")]
    InvalidPortMapping { actual: u16, expected: u16 },

    /// Failed to forward bytes from Pod.
    #[error("failed to forward bytes from Pod: {0}")]
    ForwardFromPod(#[source] futures::channel::mpsc::SendError),

    /// Failed to forward bytes to Pod.
    #[error("failed to forward bytes to Pod: {0}")]
    ForwardToPod(#[source] futures::channel::mpsc::SendError),

    /// Failed to write bytes from Pod.
    #[error("failed to write bytes from Pod: {0}")]
    WriteBytesFromPod(#[source] std::io::Error),

    /// Failed to read bytes to send to Pod.
    #[error("failed to read bytes to send to Pod: {0}")]
    ReadBytesToSend(#[source] std::io::Error),

    /// Received an error message from pod that is not a valid UTF-8.
    #[error("received invalid error message from Pod: {0}")]
    InvalidErrorMessage(#[source] std::string::FromUtf8Error),

    /// Failed to forward an error message from pod.
    #[error("failed to forward an error message {0:?}")]
    ForwardErrorMessage(String),

    /// Failed to send a WebSocket message to the server.
    #[error("failed to send a WebSocket message: {0}")]
    SendWebSocketMessage(#[source] ws::Error),

    /// Failed to receive a WebSocket message from the server.
    #[error("failed to receive a WebSocket message: {0}")]
    ReceiveWebSocketMessage(#[source] ws::Error),
}

type ErrorReceiver = oneshot::Receiver<String>;
type ErrorSender = oneshot::Sender<String>;

// Internal message used by the futures to communicate with each other.
enum Message {
    FromPod(u8, Bytes),
    ToPod(u8, Bytes),
}

struct PortforwarderState {
    waker: Option<Waker>,
    result: Option<Result<(), Error>>,
}

// Provides `AsyncRead + AsyncWrite` for each port and **does not** bind to local ports.
// Error channel for each port is only written by the server when there's an exception and
// the port cannot be used (didn't initialize or can't be used anymore).
/// Manage port forwarding.
pub struct Portforwarder {
    ports: Vec<Port>,
    state: Arc<Mutex<PortforwarderState>>,
}

impl Portforwarder {
    pub(crate) fn new<S>(stream: WebSocketStream<S>, port_nums: &[u16]) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Sized + Send + 'static,
    {
        let mut ports = Vec::new();
        let mut errors = Vec::new();
        let mut duplexes = Vec::new();
        for _ in port_nums.iter() {
            let (a, b) = tokio::io::duplex(1024 * 1024);
            let (tx, rx) = oneshot::channel();
            ports.push(Port::new(a, rx));
            errors.push(Some(tx));
            duplexes.push(b);
        }

        let state = Arc::new(Mutex::new(PortforwarderState {
            waker: None,
            result: None,
        }));
        let shared_state = state.clone();
        let port_nums = port_nums.to_owned();
        tokio::spawn(async move {
            let result = start_message_loop(stream, port_nums, duplexes, errors).await;

            let mut shared = shared_state.lock().unwrap();
            shared.result = Some(result);
            if let Some(waker) = shared.waker.take() {
                waker.wake()
            }
        });
        Portforwarder { ports, state }
    }

    /// Get streams for forwarded ports.
    pub fn ports(&mut self) -> &mut [Port] {
        self.ports.as_mut_slice()
    }
}

impl Future for Portforwarder {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap();
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }

        if let Some(waker) = &state.waker {
            if waker.will_wake(cx.waker()) {
                return Poll::Pending;
            }
        }

        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

pub struct Port {
    // Data pipe.
    stream: Option<DuplexStream>,
    // Error channel.
    error: Option<ErrorReceiver>,
}

impl Port {
    pub(crate) fn new(stream: DuplexStream, error: ErrorReceiver) -> Self {
        Port {
            stream: Some(stream),
            error: Some(error),
        }
    }

    /// Data pipe for sending to and receiving from the forwarded port.
    ///
    /// This returns a `Some` on the first call, then a `None` on every subsequent call
    pub fn stream(&mut self) -> Option<impl AsyncRead + AsyncWrite + Unpin> {
        self.stream.take()
    }

    /// Future that resolves with any error message or when the error sender is dropped.
    /// When the future resolves, the port should be considered no longer usable.
    ///
    /// This returns a `Some` on the first call, then a `None` on every subsequent call
    pub fn error(&mut self) -> Option<impl Future<Output = Option<String>>> {
        // Ignore Cancellation error.
        self.error.take().map(|recv| recv.map(|res| res.ok()))
    }
}

async fn start_message_loop<S>(
    stream: WebSocketStream<S>,
    ports: Vec<u16>,
    duplexes: Vec<DuplexStream>,
    error_senders: Vec<Option<ErrorSender>>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Sized + Send + 'static,
{
    let mut writers = Vec::new();
    // Loops to run concurrently.
    // We can spawn tasks to run `to_pod_loop` in parallel and flatten the errors, but the other 2 loops
    // are over a single WebSocket connection and cannot process each port in parallel.
    let mut loops = Vec::with_capacity(ports.len() + 2);
    // Channel to communicate with the main loop
    let (sender, receiver) = mpsc::channel::<Message>(1);
    for (i, (r, w)) in duplexes.into_iter().map(tokio::io::split).enumerate() {
        writers.push(w);
        // Each port uses 2 channels. Duplex data channel and error.
        let ch = 2 * (i as u8);
        loops.push(to_pod_loop(ch, r, sender.clone()).boxed());
    }

    let (ws_sink, ws_stream) = stream.split();
    loops.push(from_pod_loop(ws_stream, sender).boxed());
    loops.push(forwarder_loop(&ports, receiver, ws_sink, writers, error_senders).boxed());

    future::try_join_all(loops).await.map(|_| ())
}

async fn to_pod_loop(
    ch: u8,
    reader: tokio::io::ReadHalf<DuplexStream>,
    mut sender: mpsc::Sender<Message>,
) -> Result<(), Error> {
    let mut read_stream = ReaderStream::new(reader);
    while let Some(bytes) = read_stream
        .next()
        .await
        .transpose()
        .map_err(Error::ReadBytesToSend)?
    {
        if !bytes.is_empty() {
            sender
                .send(Message::ToPod(ch, bytes))
                .await
                .map_err(Error::ForwardToPod)?;
        }
    }
    Ok(())
}

async fn from_pod_loop<S>(
    mut ws_stream: futures::stream::SplitStream<WebSocketStream<S>>,
    mut sender: mpsc::Sender<Message>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Sized + Send + 'static,
{
    while let Some(msg) = ws_stream
        .next()
        .await
        .transpose()
        .map_err(Error::ReceiveWebSocketMessage)?
    {
        match msg {
            ws::Message::Binary(bin) if bin.len() > 1 => {
                let mut bytes = Bytes::from(bin);
                let ch = bytes.split_to(1)[0];
                sender
                    .send(Message::FromPod(ch, bytes))
                    .await
                    .map_err(Error::ForwardFromPod)?;
            }
            // REVIEW should we error on unexpected websocket message?
            _ => {}
        }
    }
    Ok(())
}

// Start a loop to handle messages received from other futures.
// On `Message::ToPod(ch, bytes)`, a WebSocket message is sent with the channel prefix.
// On `Message::FromPod(ch, bytes)` with an even `ch`, `bytes` are written to the port's sink.
// On `Message::FromPod(ch, bytes)` with an odd `ch`, an error message is sent to the error channel of the port.
async fn forwarder_loop<S>(
    ports: &[u16],
    mut receiver: mpsc::Receiver<Message>,
    mut ws_sink: futures::stream::SplitSink<WebSocketStream<S>, ws::Message>,
    mut writers: Vec<tokio::io::WriteHalf<DuplexStream>>,
    mut error_senders: Vec<Option<ErrorSender>>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Sized + Send + 'static,
{
    // Keep track if the channel has received the initialization frame.
    let mut initialized = vec![false; 2 * ports.len()];
    while let Some(msg) = receiver.next().await {
        match msg {
            Message::FromPod(ch, mut bytes) => {
                let ch = ch as usize;
                if ch >= initialized.len() {
                    return Err(Error::InvalidChannel(ch));
                }

                let port_index = ch / 2;
                // Initialization
                if !initialized[ch] {
                    // The initial message must be 3 bytes including the channel prefix.
                    if bytes.len() != 2 {
                        return Err(Error::InvalidInitialFrameSize);
                    }

                    let port = bytes.get_u16_le();
                    if port != ports[port_index] {
                        return Err(Error::InvalidPortMapping {
                            actual: port,
                            expected: ports[port_index],
                        });
                    }

                    initialized[ch] = true;
                    continue;
                }

                // Odd channels are for errors for (n - 1)/2 th port
                if ch % 2 != 0 {
                    // A port sends at most one error message because it's considered unusable after this.
                    if let Some(sender) = error_senders[port_index].take() {
                        let s = String::from_utf8(bytes.into_iter().collect())
                            .map_err(Error::InvalidErrorMessage)?;
                        sender.send(s).map_err(Error::ForwardErrorMessage)?;
                    }
                } else {
                    writers[port_index]
                        .write_all(&bytes)
                        .await
                        .map_err(Error::WriteBytesFromPod)?;
                }
            }

            Message::ToPod(ch, bytes) => {
                let mut bin = Vec::with_capacity(bytes.len() + 1);
                bin.push(ch);
                bin.extend(bytes.into_iter());
                ws_sink
                    .send(ws::Message::binary(bin))
                    .await
                    .map_err(Error::SendWebSocketMessage)?;
            }
        }
    }
    Ok(())
}
