use std::collections::VecDeque;
use std::marker::PhantomData;
use std::pin::Pin;

use async_tungstenite::WebSocketStream;
use futures::stream::Stream;
use futures::task::{Context, Poll};
use futures::Sink;

use chromiumoxide_cdp::cdp::browser_protocol::target::SessionId;
use chromiumoxide_types::{CallId, EventMessage, Message, MethodCall, MethodId};

use crate::error::CdpError;
use crate::error::Result;

cfg_if::cfg_if! {
    if #[cfg(feature = "async-std-runtime")] {
       use async_tungstenite::async_std::ConnectStream;
    } else if #[cfg(feature = "tokio-runtime")] {
        use async_tungstenite::tokio::ConnectStream;
    }
}
/// Exchanges the messages with the websocket
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct Connection<T: EventMessage> {
    /// Queue of commands to send.
    pending_commands: VecDeque<MethodCall>,
    /// The websocket of the chromium instance
    ws: WebSocketStream<ConnectStream>,
    /// The identifier for a specific command
    next_id: usize,
    needs_flush: bool,
    /// The message that is currently being proceessed
    pending_flush: Option<MethodCall>,
    _marker: PhantomData<T>,
}

impl<T: EventMessage + Unpin> Connection<T> {
    pub async fn connect(debug_ws_url: impl AsRef<str>) -> Result<Self> {
        cfg_if::cfg_if! {
            if #[cfg(feature = "async-std-runtime")] {
               let (ws, _) = async_tungstenite::async_std::connect_async(debug_ws_url.as_ref()).await?;
            } else if #[cfg(feature = "tokio-runtime")] {
                 let (ws, _) = async_tungstenite::tokio::connect_async(debug_ws_url.as_ref()).await?;
            }
        }

        Ok(Self {
            pending_commands: Default::default(),
            ws,
            next_id: 0,
            needs_flush: false,
            pending_flush: None,
            _marker: Default::default(),
        })
    }
}

impl<T: EventMessage> Connection<T> {
    fn next_call_id(&mut self) -> CallId {
        let id = CallId::new(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// Queue in the command to send over the socket and return the id for this
    /// command
    pub fn submit_command(
        &mut self,
        method: MethodId,
        session_id: Option<SessionId>,
        params: serde_json::Value,
    ) -> serde_json::Result<CallId> {
        let id = self.next_call_id();
        let call = MethodCall {
            id,
            method,
            session_id: session_id.map(Into::into),
            params,
        };
        self.pending_commands.push_back(call);
        Ok(id)
    }

    /// flush any processed message and start sending the next over the conn
    /// sink
    fn start_send_next(&mut self, cx: &mut Context<'_>) -> Result<()> {
        if self.needs_flush {
            if let Poll::Ready(Ok(())) = Sink::poll_flush(Pin::new(&mut self.ws), cx) {
                self.needs_flush = false;
            }
        }
        if self.pending_flush.is_none() && !self.needs_flush {
            if let Some(cmd) = self.pending_commands.pop_front() {
                // if cmd.id.to_string().contains("1") {
                //     log::error!("CMD {:?}", cmd);
                //     return Ok(())
                // }
                log::trace!("Sending {:?}", cmd);
                let msg = serde_json::to_string(&cmd)?;
                Sink::start_send(Pin::new(&mut self.ws), msg.into())?;
                self.pending_flush = Some(cmd);
            }
        }
        Ok(())
    }
}

impl<T: EventMessage + Unpin> Stream for Connection<T> {
    type Item = Result<Message<T>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();

        // queue in the next message if not currently flushing
        if let Err(err) = pin.start_send_next(cx) {
            return Poll::Ready(Some(Err(err)));
        }

        // send the message
        if let Some(call) = pin.pending_flush.take() {
            if Sink::poll_ready(Pin::new(&mut pin.ws), cx).is_ready() {
                pin.needs_flush = true;
            } else {
                pin.pending_flush = Some(call);
            }
        }
        // read from the ws
        match Stream::poll_next(Pin::new(&mut pin.ws), cx) {
            Poll::Ready(Some(Ok(msg))) => {
                return match serde_json::from_slice::<Message<T>>(&msg.into_data()) {
                    Ok(msg) => {
                        log::trace!("Received {:?}", msg);
                        Poll::Ready(Some(Ok(msg)))
                    }
                    Err(err) => {
                        log::error!("Failed to deserialize WS response {}", err);
                        Poll::Ready(Some(Err(err.into())))
                    }
                };
            }
            Poll::Ready(Some(Err(err))) => {
                return Poll::Ready(Some(Err(CdpError::Ws(err))));
            }
            _ => {}
        }
        Poll::Pending
    }
}
