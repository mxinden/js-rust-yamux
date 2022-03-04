// Adapted from https://github.com/libp2p/rust-libp2p/tree/master/transports/wasm-ext

use futures::prelude::*;
use parity_send_wrapper::SendWrapper;
use std::{error, fmt, io, mem, pin::Pin, task::Context, task::Poll};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    // Use `js_namespace` here to bind `console.log(..)` instead of just
    // `log(..)`
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

#[wasm_bindgen]
extern "C" {
    pub type Connection;

    /// Returns an iterator of JavaScript `Promise`s that resolve to `ArrayBuffer` objects
    /// (or resolve to null, see below). These `ArrayBuffer` objects contain the data that the
    /// remote has sent to us. If the remote closes the connection, the iterator must produce
    /// a `Promise` that resolves to `null`.
    #[wasm_bindgen(method, getter)]
    pub fn read(this: &Connection) -> js_sys::Iterator;

    /// Writes data to the connection. Returns a `Promise` that resolves when the connection is
    /// ready for writing again.
    ///
    /// If the `Promise` produces an error, the writing side of the connection is considered
    /// unrecoverable and the connection should be closed as soon as possible.
    ///
    /// Guaranteed to only be called after the previous write promise has resolved.
    #[wasm_bindgen(method, catch)]
    pub fn write(this: &Connection, data: &[u8]) -> Result<js_sys::Promise, JsValue>;

    /// Shuts down the writing side of the connection. After this has been called, the `write`
    /// method will no longer be called.
    #[wasm_bindgen(method, catch)]
    pub fn shutdown(this: &Connection) -> Result<(), JsValue>;

    /// Closes the connection. No other method will be called on this connection anymore.
    #[wasm_bindgen(method)]
    pub fn close(this: &Connection);
}

/// Active stream of data with a remote.
///
/// It is guaranteed that each call to `io::Write::write` on this object maps to exactly one call
/// to `write` on the FFI. In other words, no internal buffering happens for writes, and data can't
/// be split.
pub struct ExtConnection {
    /// The FFI object.
    inner: SendWrapper<Connection>,

    /// The iterator that was returned by `read()`.
    read_iterator: SendWrapper<js_sys::Iterator>,

    /// Reading part of the connection.
    read_state: ConnectionReadState,

    /// When we write data using the FFI, a promise is returned containing the moment when the
    /// underlying transport is ready to accept data again. This promise is stored here.
    /// If this is `Some`, we must wait until the contained promise is resolved to write again.
    previous_write_promise: Option<SendWrapper<JsFuture>>,
}

impl ExtConnection {
    /// Initializes a `Connection` object from the FFI connection.
    fn new(inner: Connection) -> Self {
        let read_iterator = inner.read();

        ExtConnection {
            inner: SendWrapper::new(inner),
            read_iterator: SendWrapper::new(read_iterator),
            read_state: ConnectionReadState::PendingData(Vec::new()),
            previous_write_promise: None,
        }
    }
}

/// Reading side of the connection.
enum ConnectionReadState {
    /// Some data have been read and are waiting to be transferred. Can be empty.
    PendingData(Vec<u8>),
    /// Waiting for a `Promise` containing the next data.
    Waiting(SendWrapper<JsFuture>),
    /// An error occurred or an earlier read yielded EOF.
    Finished,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Connection").finish()
    }
}

impl AsyncRead for ExtConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        loop {
            match mem::replace(&mut self.read_state, ConnectionReadState::Finished) {
                ConnectionReadState::Finished => {
                    break Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
                }

                ConnectionReadState::PendingData(ref data) if data.is_empty() => {
                    let iter_next = self.read_iterator.next().map_err(JsErr::from)?;
                    if iter_next.done() {
                        self.read_state = ConnectionReadState::Finished;
                    } else {
                        let promise: js_sys::Promise = iter_next.value().into();
                        let promise = SendWrapper::new(promise.into());
                        self.read_state = ConnectionReadState::Waiting(promise);
                    }
                    continue;
                }

                ConnectionReadState::PendingData(mut data) => {
                    debug_assert!(!data.is_empty());
                    if buf.len() <= data.len() {
                        buf.copy_from_slice(&data[..buf.len()]);
                        self.read_state =
                            ConnectionReadState::PendingData(data.split_off(buf.len()));
                        break Poll::Ready(Ok(buf.len()));
                    } else {
                        let len = data.len();
                        buf[..len].copy_from_slice(&data);
                        self.read_state = ConnectionReadState::PendingData(Vec::new());
                        break Poll::Ready(Ok(len));
                    }
                }

                ConnectionReadState::Waiting(mut promise) => {
                    let data = match Future::poll(Pin::new(&mut *promise), cx) {
                        Poll::Ready(Ok(ref data)) if data.is_null() => break Poll::Ready(Ok(0)),
                        Poll::Ready(Ok(data)) => data,
                        Poll::Ready(Err(err)) => {
                            break Poll::Ready(Err(io::Error::from(JsErr::from(err))))
                        }
                        Poll::Pending => {
                            self.read_state = ConnectionReadState::Waiting(promise);
                            break Poll::Pending;
                        }
                    };

                    // Try to directly copy the data into `buf` if it is large enough, otherwise
                    // transition to `PendingData` and loop again.
                    let data = js_sys::Uint8Array::new(&data);
                    let data_len = data.length() as usize;
                    if data_len <= buf.len() {
                        data.copy_to(&mut buf[..data_len]);
                        self.read_state = ConnectionReadState::PendingData(Vec::new());
                        break Poll::Ready(Ok(data_len));
                    } else {
                        let mut tmp_buf = vec![0; data_len];
                        data.copy_to(&mut tmp_buf[..]);
                        self.read_state = ConnectionReadState::PendingData(tmp_buf);
                        continue;
                    }
                }
            }
        }
    }
}

impl AsyncWrite for ExtConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        // Note: as explained in the doc-comments of `Connection`, each call to this function must
        // map to exactly one call to `self.inner.write()`.

        if let Some(mut promise) = self.previous_write_promise.take() {
            match Future::poll(Pin::new(&mut *promise), cx) {
                Poll::Ready(Ok(_)) => (),
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Err(io::Error::from(JsErr::from(err))))
                }
                Poll::Pending => {
                    self.previous_write_promise = Some(promise);
                    return Poll::Pending;
                }
            }
        }

        debug_assert!(self.previous_write_promise.is_none());
        self.previous_write_promise = Some(SendWrapper::new(
            self.inner.write(buf).map_err(JsErr::from)?.into(),
        ));
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // There's no flushing mechanism. In the FFI we consider that writing implicitly flushes.
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // Shutting down is considered instantaneous.
        match self.inner.shutdown() {
            Ok(()) => Poll::Ready(Ok(())),
            Err(err) => Poll::Ready(Err(io::Error::from(JsErr::from(err)))),
        }
    }
}

impl Drop for ExtConnection {
    fn drop(&mut self) {
        self.inner.close();
    }
}

/// Error that can be generated by the `ExtTransport`.
pub struct JsErr(SendWrapper<JsValue>);

impl From<JsValue> for JsErr {
    fn from(val: JsValue) -> JsErr {
        JsErr(SendWrapper::new(val))
    }
}

impl From<JsErr> for io::Error {
    fn from(err: JsErr) -> io::Error {
        io::Error::new(io::ErrorKind::Other, err.to_string())
    }
}

impl fmt::Debug for JsErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl fmt::Display for JsErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(s) = self.0.as_string() {
            write!(f, "{}", s)
        } else if let Some(err) = self.0.dyn_ref::<js_sys::Error>() {
            write!(f, "{}", String::from(err.message()))
        } else if let Some(obj) = self.0.dyn_ref::<js_sys::Object>() {
            write!(f, "{}", String::from(obj.to_string()))
        } else {
            write!(f, "{:?}", &*self.0)
        }
    }
}

impl error::Error for JsErr {}

#[wasm_bindgen]
pub struct YamuxConnection {
    inner: yamux::Connection<ExtConnection>,
}

#[wasm_bindgen]
impl YamuxConnection {
    #[wasm_bindgen(constructor)]
    pub fn new(connection: Connection) -> YamuxConnection {
        log("New connection.");
        let socket = ExtConnection::new(connection);
        let connection =
            yamux::Connection::new(socket, yamux::Config::default(), yamux::Mode::Server);
        Self { inner: connection }
    }
}
