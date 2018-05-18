use {util, TIMESTAMP_MASK};
use delays::Delays;
use in_queue::InQueue;
use out_queue::OutQueue;
use packet::{self, Packet};

//use mio::net::UdpSocket;
use arraydeque::ArrayDeque;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::{Handle, PollEvented, Remote, Timeout};
use tokio_io::{AsyncRead, AsyncWrite};
use mio::{Ready, Registration, SetReadiness};
use futures::{Async, AsyncSink, Future, Sink, Stream};
use futures::sync::oneshot;
use future_utils::mpsc::{self, UnboundedReceiver, UnboundedSender};
use void::Void;

use bytes::{BufMut, Bytes, BytesMut};
use slab::Slab;

use std::{cmp, fmt, io, mem, u32};
use std::sync::{Arc, RwLock};
use std::net::SocketAddr;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A uTP socket. Can be used to make outgoing connections.
pub struct UtpSocket {
    // Shared state
    inner: InnerCell,
    req_finalize: oneshot::Sender<oneshot::Sender<()>>,
}

impl fmt::Debug for UtpSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "UtpSocket")
    }
}

/// A uTP stream.
pub struct UtpStream {
    // Shared state
    inner: InnerCell,

    // Connection identifier
    token: usize,

    // Mio registration
    registration: PollEvented<Registration>,
}

impl fmt::Debug for UtpStream {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter
            .debug_struct("UtpStream")
            .field("peer_addr", &self.peer_addr())
            .finish()
    }
}

/// Listens for incoming uTP connections.
pub struct UtpListener {
    // Shared state
    inner: InnerCell,

    // Used to register interest in accepting UTP sockets
    registration: PollEvented<Registration>,
}

impl fmt::Debug for UtpListener {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "UtpListener")
    }
}

pub struct RawReceiver {
    inner: InnerCell,
    channel_rx: UnboundedReceiver<RawChannel>,
}

pub struct RawChannel {
    inner: InnerCell,
    peer_addr: SocketAddr,
    bytes_rx: UnboundedReceiver<BytesMut>,
    registration: PollEvented<Registration>,
}

// Shared between the UtpSocket and each UtpStream
struct Inner {
    // State that needs to be passed to `Connection`. This is broken out to make
    // the borrow checker happy.
    shared: Shared,

    // Handle to the event loop.
    remote: Remote,

    // Connection specific state
    connections: Slab<Connection>,

    // Lookup a connection by key
    connection_lookup: HashMap<Key, usize>,

    // Buffer used for in-bound data
    in_buf: BytesMut,

    accept_buf: VecDeque<UtpStream>,

    listener: SetReadiness,

    listener_open: bool,

    // Reset packets to be sent. These packets are not associated with any connection. They
    // don't need to be acknowledged either.
    reset_packets: ArrayDeque<[(Packet, SocketAddr); MAX_CONNECTIONS_PER_SOCKET]>,

    raw_data_max: usize,
    raw_data_buffered: usize,
    raw_receiver: Option<UnboundedSender<RawChannel>>,
    raw_channels: HashMap<SocketAddr, (UnboundedSender<BytesMut>, SetReadiness)>,
}

type InnerCell = Arc<RwLock<Inner>>;

unsafe impl Send for Inner {}

struct Shared {
    // The UDP socket backing everything!
    socket: UdpSocket,

    // The current readiness of the socket, this is used when figuring out the
    // readiness of each connection.
    ready: Ready,
}

// Owned by UtpSocket
#[derive(Debug)]
struct Connection {
    // Current socket state
    state: State,

    // A combination of the send ID and the socket address
    key: Key,

    // True when the `UtpStream` handle has been dropped
    released: bool,

    // False when `shutdown_write` has been called.
    write_open: bool,
    // False when we've recieved a FIN.
    read_open: bool,

    // Used to signal readiness on the `UtpStream`
    set_readiness: SetReadiness,

    // Queue of outbound packets. Packets will stay in the queue until the peer
    // has acked them.
    out_queue: OutQueue,

    // Queue of inbound packets. The queue orders packets according to their
    // sequence number.
    in_queue: InQueue,

    // Activity deadline
    deadline: Option<Instant>,
    // Activitity disconnect deadline
    last_recv_time: Instant,
    disconnect_timeout_secs: u32,

    // Tracks delays for the congestion control algorithm
    our_delays: Delays,

    their_delays: Delays,

    last_maxed_out_window: Instant,
    average_delay: i32,
    current_delay_sum: i64,
    current_delay_samples: i64,
    average_delay_base: u32,
    average_sample_time: Instant,
    clock_drift: i32,
    slow_start: bool,
    // Artifical packet loss rate (for testing)
    // Probability of dropping is loss_rate / u32::MAX
    //#[cfg(test)]
    //loss_rate: u32,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct Key {
    receive_id: u16,
    addr: SocketAddr,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum State {
    // Establishing a new connection, waiting for the peer to respond with a
    // STATE.
    SynSent,
    // Received Syn, the state packet is sent immediately, but the connection is
    // not transitioned to `Connected` until it has been accepted.
    SynRecv,
    // Fully connected state
    Connected,
    // A SYN has been sent and we are currently waiting for an ACK before
    // closing the connection.
    FinSent,
    // The connection has been reset by the remote.
    Reset,
}

const DEFAULT_RAW_DATA_MAX: usize = 1_024 * 1_024;
const MIN_BUFFER_SIZE: usize = 4 * 1_024;
const DEFAULT_IN_BUFFER_SIZE: usize = 64 * 1024;
const MAX_CONNECTIONS_PER_SOCKET: usize = 2 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 1_000;
const TARGET_DELAY: u32 = 100_000; // 100ms in micros
const DEFAULT_DISCONNECT_TIMEOUT_SECS: u32 = 60;

const SLOW_START_THRESHOLD: usize = DEFAULT_IN_BUFFER_SIZE;
const MAX_CWND_INCREASE_BYTES_PER_RTT: usize = 3000;
const MIN_WINDOW_SIZE: usize = 10;
const MAX_DATA_SIZE: usize = 1_400 - 20;

impl UtpSocket {
    /// Bind a new `UtpSocket` to the given socket address
    pub fn bind(addr: &SocketAddr, handle: &Handle) -> io::Result<(UtpSocket, UtpListener)> {
        UdpSocket::bind(addr, handle).and_then(|s| UtpSocket::from_socket(s, handle))
    }

    /// Gets the local address that the socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        unwrap!(self.inner.read()).shared.socket.local_addr()
    }

    /// Create a new `Utpsocket` backed by the provided `UdpSocket`.
    pub fn from_socket(socket: UdpSocket, handle: &Handle) -> io::Result<(UtpSocket, UtpListener)> {
        let (listener_registration, listener_set_readiness) = Registration::new2();
        let (finalize_tx, finalize_rx) = oneshot::channel();

        let inner = Inner::new_shared(handle, socket, listener_set_readiness);
        let listener = UtpListener {
            inner: inner.clone(),
            registration: PollEvented::new(listener_registration, handle)?,
        };

        let socket = UtpSocket {
            inner: inner.clone(),
            req_finalize: finalize_tx,
        };

        let next_tick = Instant::now() + Duration::from_millis(500);
        let timeout = Timeout::new_at(next_tick, handle)?;
        let refresher = SocketRefresher {
            inner: inner,
            next_tick: next_tick,
            timeout: timeout,
            req_finalize: finalize_rx,
        };
        handle.spawn(refresher);

        Ok((socket, listener))
    }

    /// Connect a new `UtpSocket` to the given remote socket address
    pub fn connect(&self, addr: &SocketAddr) -> UtpStreamConnect {
        let state = match unwrap!(self.inner.write()).connect(addr, &self.inner) {
            Ok(stream) => UtpStreamConnectState::Waiting(stream),
            Err(e) => UtpStreamConnectState::Err(e),
        };
        UtpStreamConnect { state }
    }

    /*
    /// Called whenever the socket readiness changes
    /// Returns true if all connections have been finalised.
    pub fn ready(&self, ready: Ready) -> io::Result<bool> {
        unwrap!(self.inner.write()).ready(ready, &self.inner)
    }

    /// This function should be called every 500ms
    pub fn tick(&self) -> io::Result<()> {
        unwrap!(self.inner.write()).tick()
    }
    */

    /// Consume the socket and the convert it to a future which resolves once all connections have
    /// been closed gracefully.
    pub fn finalize(self) -> UtpSocketFinalize {
        let (respond_tx, respond_rx) = oneshot::channel();
        unwrap!(self.req_finalize.send(respond_tx));
        UtpSocketFinalize {
            resp_finalize: respond_rx,
        }
    }

    pub fn raw_receiver(&self) -> RawReceiver {
        let (tx, rx) = mpsc::unbounded();
        let ret = RawReceiver {
            inner: self.inner.clone(),
            channel_rx: rx,
        };
        let mut inner = unwrap!(self.inner.write());
        // NOTE: See the comments in RawReceiver::drop about removing this restriction
        assert!(
            inner.raw_receiver.is_none(),
            "cannot have two raw receivers simultaneously"
        );
        inner.raw_receiver = Some(tx);
        ret
    }

    pub fn raw_channel(&self, addr: &SocketAddr) -> io::Result<RawChannel> {
        let mut inner = unwrap!(self.inner.write());
        inner.raw_channel(*addr, &self.inner, None)
    }
}

impl Stream for RawReceiver {
    type Item = RawChannel;
    type Error = Void;

    fn poll(&mut self) -> Result<Async<Option<RawChannel>>, Void> {
        self.channel_rx.poll()
    }
}

impl Drop for RawReceiver {
    fn drop(&mut self) {
        let mut inner = unwrap!(self.inner.write());
        /*
         *  for this logic to work we need a way to drain an UnboundedReceiver from outsute a Task
         *
        let mut channels = Vec::new();
        loop {
            match self.channel_rx.poll().void_unwrap() {
                Async::Ready(Some(raw_channel)) => {
                    channels.push(raw_channel);
                },
                Async::Ready(None) => {
                    break;
                },
                Async::NotReady => {
                    // NOTE: we only de-register the channel if the sender is still alive
                    // (indicating it is still registered with the Inner).
                    inner.raw_receiver = None;
                    break;
                },
            }
        }
        // NOTE: Must drop inner first to avoid deadlocking
        drop(inner);
        drop(channels);

        */

        // for now, assume we are the only RawReciever
        inner.raw_receiver = None;
    }
}

impl RawChannel {
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
}

impl Drop for RawChannel {
    fn drop(&mut self) {
        let mut inner = unwrap!(self.inner.write());
        /*
         * Similarly to RawReceiver, we need a .pop() method on bytes_rx before we can implement
         * this. Otherwise it can crash when dropped outside of a task.
        loop {
            match self.bytes_rx.poll().void_unwrap() {
                Async::Ready(Some(bytes)) => {
                    inner.raw_data_buffered -= bytes.len();
                },
                Async::Ready(None) => {
                    break;
                },
                Async::NotReady => {
                    // NOTE: we only de-register the channel if the sender is still alive
                    // (indicating it is still registered with the Inner).
                    let _ = inner.raw_channels.remove(&self.peer_addr);
                    break;
                },
            }
        }
        */

        // For now, assume we are the only raw_channel for this address
        let _ = inner.raw_channels.remove(&self.peer_addr);
    }
}

impl Stream for RawChannel {
    type Item = BytesMut;
    type Error = Void;

    fn poll(&mut self) -> Result<Async<Option<BytesMut>>, Void> {
        let ret = self.bytes_rx.poll();
        if let Ok(Async::Ready(Some(ref bytes))) = ret {
            let mut inner = unwrap!(self.inner.write());
            inner.raw_data_buffered -= bytes.len();
        }
        ret
    }
}

impl Sink for RawChannel {
    type SinkItem = Bytes;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Bytes) -> io::Result<AsyncSink<Bytes>> {
        match self.registration.poll_write() {
            Async::NotReady => Ok(AsyncSink::NotReady(item)),
            Async::Ready(()) => {
                let res = {
                    let inner = unwrap!(self.inner.read());
                    inner.shared.socket.send_to(&item[..], &self.peer_addr)
                };
                match res {
                    Ok(n) => {
                        assert_eq!(n, item.len());
                        Ok(AsyncSink::Ready)
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.registration.need_write();
                        Ok(AsyncSink::NotReady(item))
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    fn poll_complete(&mut self) -> io::Result<Async<()>> {
        Ok(Async::Ready(()))
    }
}

/*
impl Evented for UtpSocket {
    fn register(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        unwrap!(self.inner.write()).shared.socket.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        unwrap!(self.inner.write()).shared.socket.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        unwrap!(self.inner.write()).shared.socket.deregister(poll)
    }
}
*/

impl UtpListener {
    /// Get the local address that the listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        unwrap!(self.inner.read()).shared.socket.local_addr()
    }

    /// Receive a new inbound connection.
    ///
    /// This function will also advance the state of all associated connections.
    pub fn accept(&self) -> io::Result<UtpStream> {
        if let Async::NotReady = self.registration.poll_read() {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        match unwrap!(self.inner.write()).accept() {
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.registration.need_read();
                }
                Err(e)
            }
            Ok(stream) => Ok(stream),
        }
    }

    /// Convert the `UtpListener` to a stream of incoming connections.
    pub fn incoming(self) -> Incoming {
        Incoming { listener: self }
    }
}

impl Drop for UtpListener {
    fn drop(&mut self) {
        let mut inner = unwrap!(self.inner.write());
        inner.listener_open = false;

        // Empty the connection queue
        let mut streams = Vec::new();
        while let Ok(stream) = inner.accept() {
            streams.push(stream);
        }

        // Must release the lock before dropping the streams
        drop(inner);
        drop(streams);
    }
}

#[cfg(test)]
impl UtpListener {
    pub fn is_readable(&self) -> bool {
        let inner = unwrap!(self.inner.read());
        inner.listener.readiness().is_readable()
    }
}

/*
impl Evented for UtpListener {
    fn register(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        self.registration.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        self.registration.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        Evented::deregister(&self.registration, poll)
    }
}
*/

impl UtpStream {
    /// Get the address of the remote peer.
    pub fn peer_addr(&self) -> SocketAddr {
        let inner = unwrap!(self.inner.read());
        let connection = &inner.connections[self.token];
        connection.key.addr
    }

    /// Get the local address that the stream is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let inner = unwrap!(self.inner.read());
        inner.shared.socket.local_addr()
    }

    /// The same as `Read::read` except it does not require a mutable reference to the stream.
    pub fn read_immutable(&self, dst: &mut [u8]) -> io::Result<usize> {
        if let Async::NotReady = self.registration.poll_read() {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        let mut inner = unwrap!(self.inner.write());
        let connection = &mut inner.connections[self.token];

        match connection.in_queue.read(dst) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.registration.need_read();
                if connection.state == State::Connected && connection.read_open {
                    connection.update_readiness()?;
                    Err(io::ErrorKind::WouldBlock.into())
                } else if connection.state == State::Reset {
                    Err(io::ErrorKind::ConnectionReset.into())
                } else {
                    Ok(0)
                }
            }
            ret => {
                connection.update_local_window();
                ret
            }
        }
    }

    /// The same as `Write::write` except it does not require a mutable reference to the stream.
    pub fn write_immutable(&self, src: &[u8]) -> io::Result<usize> {
        if let Async::NotReady = self.registration.poll_write() {
            return Err(io::ErrorKind::WouldBlock.into());
        }

        match unwrap!(self.inner.write()).write(self.token, src) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.registration.need_write();
                Err(io::ErrorKind::WouldBlock.into())
            }
            ret => ret,
        }
    }

    /// Shutdown the write-side of the uTP connection. The stream can still be used to read data
    /// received from the peer but can no longer be used to send data. Will cause the peer to
    /// receive and EOF.
    pub fn shutdown_write(&self) -> io::Result<()> {
        unwrap!(self.inner.write()).shutdown_write(self.token)
    }

    /// Flush all outgoing data on the socket. Returns `Err(WouldBlock)` if there remains data that
    /// could not be immediately written.
    pub fn flush_immutable(&self) -> io::Result<()> {
        if !unwrap!(self.inner.write()).flush(self.token)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    /// Sets how long we must lose contact with the remote peer for before we consider the
    /// connection to have died. Defaults to 1 minute.
    pub fn set_disconnect_timeout(&self, duration: Duration) {
        let mut inner = unwrap!(self.inner.write());
        let connection = &mut inner.connections[self.token];
        connection.disconnect_timeout_secs =
            cmp::min(u64::from(u32::MAX), duration.as_secs()) as u32;
    }
}

#[cfg(test)]
impl UtpStream {
    pub fn is_readable(&self) -> bool {
        let inner = unwrap!(self.inner.read());
        let connection = &inner.connections[self.token];
        connection.set_readiness.readiness().is_readable()
    }

    pub fn is_writable(&self) -> bool {
        let inner = unwrap!(self.inner.read());
        let connection = &inner.connections[self.token];
        connection.set_readiness.readiness().is_writable()
    }

    /*
    pub fn set_loss_rate(&self, rate: f32) {
        let mut inner = unwrap!(self.inner.write());
        let connection = &mut inner.connections[self.token];
        connection.loss_rate = (rate as f64 * ::std::u32::MAX as f64) as u32;
    }
    */
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        unwrap!(self.inner.write()).close(self.token);
    }
}

/*
impl Evented for UtpStream {
    fn register(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        self.registration.register(poll, token, interest, opts)
    }

    fn reregister(&self, poll: &Poll, token: Token, interest: Ready, opts: PollOpt)
        -> io::Result<()>
    {
        self.registration.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        Evented::deregister(&self.registration, poll)
    }
}
*/

impl Inner {
    fn new(handle: &Handle, socket: UdpSocket, listener_set_readiness: SetReadiness) -> Inner {
        Inner {
            shared: Shared {
                socket: socket,
                ready: Ready::empty(),
            },
            remote: handle.remote().clone(),
            connections: Slab::new(),
            connection_lookup: HashMap::new(),
            in_buf: BytesMut::with_capacity(DEFAULT_IN_BUFFER_SIZE),
            accept_buf: VecDeque::new(),
            listener: listener_set_readiness,
            listener_open: true,
            reset_packets: ArrayDeque::new(),
            raw_data_max: DEFAULT_RAW_DATA_MAX,
            raw_data_buffered: 0,
            raw_receiver: None,
            raw_channels: HashMap::new(),
        }
    }

    fn new_shared(
        handle: &Handle,
        socket: UdpSocket,
        listener_set_readiness: SetReadiness,
    ) -> InnerCell {
        Arc::new(RwLock::new(Inner::new(
            handle,
            socket,
            listener_set_readiness,
        )))
    }

    fn accept(&mut self) -> io::Result<UtpStream> {
        match self.accept_buf.pop_front() {
            Some(socket) => {
                let conn = &mut self.connections[socket.token];

                if conn.state == State::SynRecv {
                    conn.state = State::Connected;
                } else if conn.state.is_closed() {
                    // Connection is being closed, but there may be data in the
                    // buffer...
                } else {
                    unreachable!();
                }

                conn.update_readiness()?;

                Ok(socket)
            }
            None => {
                // Unset readiness
                self.listener.set_readiness(Ready::empty())?;

                Err(io::ErrorKind::WouldBlock.into())
            }
        }
    }

    fn write(&mut self, token: usize, src: &[u8]) -> io::Result<usize> {
        let conn = &mut self.connections[token];

        if conn.state.is_closed() || !conn.write_open {
            return Err(io::ErrorKind::BrokenPipe.into());
        }

        match conn.out_queue.write(src) {
            Ok(n) => {
                conn.flush(&mut self.shared)?;
                conn.update_readiness()?;
                Ok(n)
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                conn.last_maxed_out_window = Instant::now();
                conn.update_readiness()?;
                Err(io::ErrorKind::WouldBlock.into())
            }
            Err(e) => Err(e),
        }
    }

    /// Connect a new `UtpSocket` to the given remote socket address
    fn connect(&mut self, addr: &SocketAddr, inner: &InnerCell) -> io::Result<UtpStream> {
        if self.connections.len() >= MAX_CONNECTIONS_PER_SOCKET {
            debug_assert!(self.connections.len() <= MAX_CONNECTIONS_PER_SOCKET);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "socket has max connections",
            ));
        }

        // The peer establishing the connection picks the identifiers uses for
        // the stream.
        let (receive_id, mut send_id) = util::generate_sequential_identifiers();

        let mut key = Key {
            receive_id,
            addr: *addr,
        };

        // Because the IDs are randomly generated, there could already be an
        // existing connection with the key, so sequentially scan until we hit a
        // free slot.
        while self.connection_lookup.contains_key(&key) {
            key.receive_id += 1;
            send_id += 1;
        }

        let handle = unwrap!(
            self.remote.handle(),
            "cannot be used outside of the event loop!"
        );
        let (registration, set_readiness) = Registration::new2();
        let registration = PollEvented::new(registration, &handle)?;

        let mut connection = Connection::new_outgoing(key.clone(), set_readiness, send_id);
        connection.flush(&mut self.shared)?;

        let token = self.connections.insert(connection);

        // Track the connection in the lookup
        self.connection_lookup.insert(key, token);

        Ok(UtpStream {
            inner: inner.clone(),
            token: token,
            registration: registration,
        })
    }

    fn shutdown_write(&mut self, token: usize) -> io::Result<()> {
        let conn = &mut self.connections[token];
        conn.write_open = false;
        if !conn.state.is_closed() {
            conn.out_queue.push(Packet::fin());
            conn.flush(&mut self.shared)?;
        }
        Ok(())
    }

    fn close(&mut self, token: usize) {
        let finalized = {
            let conn = &mut self.connections[token];
            conn.released = true;
            if !conn.state.is_closed() {
                conn.out_queue.push(Packet::fin());
                conn.state = State::FinSent;
                //let _ = conn.flush(&mut self.shared);
            }
            conn.is_finalized()
        };

        if finalized {
            self.remove_connection(token);
        }
    }

    fn ready(&mut self, ready: Ready, inner: &InnerCell) -> io::Result<bool> {
        trace!("ready; ready={:?}", ready);

        // Update readiness
        self.shared.update_ready(ready);

        loop {
            // Try to receive a packet
            let (bytes, addr) = match self.recv_from() {
                Ok(v) => v,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    trace!("ready -> would block");
                    break;
                }
                Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    continue;
                }
                Err(e) => {
                    trace!("recv_from; error={:?}", e);
                    return Err(e);
                }
            };

            //trace!("recv_from; addr={:?}; packet={:?}", addr, b);

            match self.process(bytes, addr, inner) {
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    panic!("NOPE");
                }
                Err(e) => return Err(e),
            }
        }

        let still_writable = self.flush_all()?;
        if still_writable {
            for &(_, ref set_readiness) in self.raw_channels.values() {
                set_readiness.set_readiness(ready)?;
            }
        }
        let _ = self.flush_reset_packets()?;
        Ok(self.connection_lookup.is_empty())
    }

    fn refresh(&mut self, inner: &InnerCell) -> io::Result<Async<bool>> {
        let mut ready = Ready::empty();
        if let Async::Ready(()) = self.shared.socket.poll_read() {
            ready |= Ready::readable();
        }
        if let Async::Ready(()) = self.shared.socket.poll_write() {
            ready |= Ready::writable();
        }
        if ready.is_empty() {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(self.ready(ready, inner)?))
        }
    }

    // Note, it looks hairy to me that inner instance has to be passed to the method of Inner.
    // I just copied this pattern fromm `Inner::refresh()`. But probably this should be reworked,
    // if possible.
    fn tick(&mut self, inner: &InnerCell) -> io::Result<()> {
        trace!("Socket::tick");
        for &idx in self.connection_lookup.values() {
            self.connections[idx].tick(&mut self.shared, inner)?;
        }

        Ok(())
    }

    fn process(&mut self, bytes: BytesMut, addr: SocketAddr, inner: &InnerCell) -> io::Result<()> {
        let packet = match Packet::parse(bytes) {
            Ok(packet) => packet,
            Err(bytes) => return self.process_raw(bytes, addr, inner),
        };
        // Process the packet
        match packet.ty() {
            packet::Type::Syn => {
                // SYN packets are special
                self.process_syn(&packet, addr, inner)
            }
            _ => {
                // All other packets are associated with a connection, and as
                // such they should be sequenced.
                let key = Key::new(packet.connection_id(), addr);

                match self.connection_lookup.get(&key) {
                    Some(&token) => {
                        let finalized = {
                            let conn = &mut self.connections[token];
                            conn.process(packet, &mut self.shared)?
                        };

                        if finalized {
                            self.remove_connection(token);
                        }

                        Ok(())
                    }
                    None => self.process_unknown(packet, addr, inner),
                }
            }
        }
    }

    /// Handle packets with unknown ID.
    fn process_unknown(
        &mut self,
        packet: Packet,
        addr: SocketAddr,
        inner: &InnerCell,
    ) -> io::Result<()> {
        trace!("no connection associated with ID; treating as raw data");

        if packet.ty() != packet::Type::Reset {
            self.schedule_reset(packet.connection_id(), addr);
        }
        self.process_raw(packet.into_bytes(), addr, inner)
    }

    fn process_syn(
        &mut self,
        packet: &Packet,
        addr: SocketAddr,
        inner: &InnerCell,
    ) -> io::Result<()> {
        if !self.listener_open || self.connections.len() >= MAX_CONNECTIONS_PER_SOCKET {
            debug_assert!(self.connections.len() <= MAX_CONNECTIONS_PER_SOCKET);
            self.schedule_reset(packet.connection_id(), addr);
            return Ok(());
        }

        let send_id = packet.connection_id();
        let receive_id = send_id + 1;
        let key = Key { receive_id, addr };

        if self.connection_lookup.contains_key(&key) {
            // Just ignore the packet...
            return Ok(());
        }

        let handle = unwrap!(
            self.remote.handle(),
            "cannot be used outside of the event loop!"
        );
        let (registration, set_readiness) = Registration::new2();
        let registration = PollEvented::new(registration, &handle)?;

        let mut connection =
            Connection::new_incoming(key.clone(), set_readiness, send_id, packet.seq_nr());
        // This will handle the state packet being sent
        connection.flush(&mut self.shared)?;

        let token = self.connections.insert(connection);
        self.connection_lookup.insert(key, token);

        // Store the connection in the accept buffer
        self.accept_buf.push_back(UtpStream {
            inner: inner.clone(),
            token: token,
            registration: registration,
        });

        // Notify the listener
        self.listener.set_readiness(Ready::readable())?;

        Ok(())
    }

    fn process_raw(
        &mut self,
        bytes: BytesMut,
        addr: SocketAddr,
        inner: &InnerCell,
    ) -> io::Result<()> {
        if self.raw_data_buffered + bytes.len() > self.raw_data_max {
            return Ok(());
        }
        self.raw_data_buffered += bytes.len();

        if let Some(&(ref channel, _)) = self.raw_channels.get(&addr) {
            let _ = channel.unbounded_send(bytes);
            return Ok(());
        }

        if self.raw_receiver.is_some() {
            let channel = self.raw_channel(addr, inner, Some(bytes))?;
            unwrap!(unwrap!(self.raw_receiver.as_ref()).unbounded_send(channel));
        }

        Ok(())
    }

    fn raw_channel(
        &mut self,
        addr: SocketAddr,
        inner: &InnerCell,
        data: Option<BytesMut>,
    ) -> io::Result<RawChannel> {
        let handle = unwrap!(
            self.remote.handle(),
            "cannot be used outside of the event loop!"
        );
        let (registration, set_readiness) = Registration::new2();
        let registration = PollEvented::new(registration, &handle)?;
        let (tx, rx) = mpsc::unbounded();
        if let Some(data) = data {
            let _ = tx.unbounded_send(data);
        }
        let ret = RawChannel {
            inner: inner.clone(),
            peer_addr: addr,
            bytes_rx: rx,
            registration: registration,
        };
        // NOTE: See the comments in RawChannel::drop about removing this restriction
        assert!(
            self.raw_channels
                .insert(addr, (tx, set_readiness))
                .is_none(),
            "already have a raw channel for this address"
        );
        Ok(ret)
    }

    fn recv_from(&mut self) -> io::Result<(BytesMut, SocketAddr)> {
        // Ensure the buffer has at least 4kb of available space.
        self.in_buf.reserve(MIN_BUFFER_SIZE);

        // Read in the bytes
        let addr = unsafe {
            let (n, addr) = self.shared.socket.recv_from(self.in_buf.bytes_mut())?;
            self.in_buf.advance_mut(n);
            addr
        };

        let bytes = self.in_buf.take();

        Ok((bytes, addr))
    }

    /// Returns true, if socket is still ready to write.
    fn flush_all(&mut self) -> io::Result<bool> {
        if self.connection_lookup.is_empty() {
            return Ok(true);
        }

        // Iterate in semi-random order so that bandwidth is divided fairly between connections.
        let skip_point = util::rand::<usize>() % self.connection_lookup.len();
        let tokens = self.connection_lookup
            .values()
            .skip(skip_point)
            .chain(self.connection_lookup.values().take(skip_point));
        for &token in tokens {
            let conn = &mut self.connections[token];
            if !conn.flush(&mut self.shared)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Enqueues Reset packet with given information.
    fn schedule_reset(&mut self, conn_id: u16, dest_addr: SocketAddr) {
        // Send the RESET packet, ignoring errors...
        let mut p = Packet::reset();
        p.set_connection_id(conn_id);
        let _ = self.reset_packets.push_back((p, dest_addr));
    }

    /// Attempts to send enqueued Reset packets.
    fn flush_reset_packets(&mut self) -> Result<Async<()>, io::Error> {
        while let Some((packet, dest_addr)) = self.reset_packets.pop_front() {
            match self.shared.socket.send_to(packet.as_slice(), &dest_addr) {
                Ok(n) => {
                    // should never fail!
                    assert_eq!(n, packet.as_slice().len());
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.shared.need_writable();
                    let _ = self.reset_packets.push_back((packet, dest_addr));
                    return Ok(Async::NotReady);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        Ok(Async::Ready(()))
    }

    fn flush(&mut self, token: usize) -> io::Result<bool> {
        let connection = &mut self.connections[token];
        connection.flush(&mut self.shared)
    }

    fn remove_connection(&mut self, token: usize) {
        let connection = self.connections.remove(token);
        self.connection_lookup.remove(&connection.key);
        trace!(
            "removing connection state; token={:?}, addr={:?}; id={:?}",
            token,
            connection.key.addr,
            connection.key.receive_id
        );
    }
}

impl Shared {
    fn update_ready(&mut self, ready: Ready) {
        self.ready |= ready;
    }

    fn is_writable(&self) -> bool {
        self.ready.is_writable()
    }

    fn need_writable(&mut self) {
        self.ready.remove(Ready::writable());
    }
}

impl Connection {
    fn new(
        state: State,
        key: Key,
        set_readiness: SetReadiness,
        out_queue: OutQueue,
        in_queue: InQueue,
        deadline_after: Option<u64>,
    ) -> Self {
        let now = Instant::now();
        let deadline = deadline_after.map(|millis| now + Duration::from_millis(millis));
        Self {
            state,
            key,
            set_readiness,
            out_queue,
            in_queue,
            our_delays: Delays::new(),
            their_delays: Delays::new(),
            released: false,
            write_open: true,
            read_open: true,
            deadline,
            last_recv_time: now,
            disconnect_timeout_secs: DEFAULT_DISCONNECT_TIMEOUT_SECS,
            last_maxed_out_window: now,
            average_delay: 0,
            current_delay_sum: 0,
            current_delay_samples: 0,
            average_delay_base: 0,
            average_sample_time: now,
            clock_drift: 0,
            slow_start: true,
            //#[cfg(test)]
            //loss_rate: 0,
        }
    }

    /// Constructs new connection that we initiated with `UtpSocket::connect()`.
    fn new_outgoing(key: Key, set_readiness: SetReadiness, send_id: u16) -> Self {
        // SYN packet has seq_nr of 1
        let mut out_queue = OutQueue::new(send_id, 0, None);

        let mut packet = Packet::syn();
        packet.set_connection_id(key.receive_id);

        // Queue the syn packet
        out_queue.push(packet);

        Self::new(
            State::SynSent,
            key,
            set_readiness,
            out_queue,
            InQueue::new(None),
            Some(DEFAULT_TIMEOUT_MS),
        )
    }

    /// Constructs new incoming connection from Syn packet.
    fn new_incoming(key: Key, set_readiness: SetReadiness, send_id: u16, ack_nr: u16) -> Self {
        let seq_nr = util::rand();
        let out_queue = OutQueue::new(send_id, seq_nr, Some(ack_nr));
        let in_queue = InQueue::new(Some(ack_nr));
        Self::new(
            State::SynRecv,
            key,
            set_readiness,
            out_queue,
            in_queue,
            None,
        )
    }

    fn update_local_window(&mut self) {
        self.out_queue
            .set_local_window(self.in_queue.local_window());
    }

    /// Process an inbound packet for the connection
    fn process(&mut self, packet: Packet, shared: &mut Shared) -> io::Result<bool> {
        let now = Instant::now();

        if self.state == State::Reset {
            return Ok(self.is_finalized());
        }

        if packet.ty() == packet::Type::Reset {
            self.state = State::Reset;

            // Update readiness
            self.update_readiness()?;

            return Ok(self.is_finalized());
        }

        // TODO: Invalid packets should be discarded here.

        self.update_delays(now, &packet);

        if packet.ty() == packet::Type::State {
            // State packets are special, they do not have an associated
            // sequence number, thus do not require ordering. They are only used
            // to ACK packets, which is handled above, and to transition a
            // connection into the connected state.
            if self.state == State::SynSent {
                self.in_queue.set_initial_ack_nr(packet.seq_nr());
                self.out_queue.set_peer_window(packet.wnd_size());

                self.state = State::Connected;
            }
        } else {
            // TODO: validate the packet's ack_nr

            // Add the packet to the inbound queue. This handles ordering
            trace!("inqueue -- push packet");
            if !self.in_queue.push(packet) {
                // Invalid packet, avoid any further processing
                trace!("invalid packet");
                return Ok(false);
            }
        }

        // TODO: count duplicate ACK counter

        trace!("polling from in_queue");

        while let Some(packet) = self.in_queue.poll() {
            trace!("process; packet={:?}; state={:?}", packet, self.state);

            // Update the peer window size
            self.out_queue.set_peer_window(packet.wnd_size());

            // At this point, we only receive CTL frames. Data is held in the
            // queue
            match packet.ty() {
                packet::Type::Reset => {
                    self.state = State::Reset;
                }
                packet::Type::Fin => {
                    self.read_open = false;
                }
                packet::Type::Data | packet::Type::Syn | packet::Type::State => unreachable!(),
            }
        }

        trace!(
            "updating local window, acks; window={:?}; ack={:?}",
            self.in_queue.local_window(),
            self.in_queue.ack_nr()
        );

        self.update_local_window();
        let (ack_nr, selective_acks) = self.in_queue.ack_nr();
        self.out_queue.set_local_ack(ack_nr, selective_acks);
        self.last_recv_time = Instant::now();

        // Reset the timeout
        self.reset_timeout();

        // Flush out queue
        self.flush(shared)?;

        // Update readiness
        self.update_readiness()?;

        Ok(self.is_finalized())
    }

    /// Returns true, if socket is still ready to write.
    fn flush(&mut self, shared: &mut Shared) -> io::Result<bool> {
        let mut sent = false;

        if self.state == State::Reset {
            return Ok(true);
        }

        while let Some(next) = self.out_queue.next() {
            if !shared.is_writable() {
                return Ok(false);
            }

            trace!(
                "send_to; addr={:?}; packet={:?}",
                self.key.addr,
                next.packet()
            );

            // We randomly drop packets when testing.
            //#[cfg(test)]
            //let drop_packet = self.loss_rate >= util::rand();
            //#[cfg(not(test))]
            let drop_packet = false;

            if drop_packet {
                next.sent();
                sent = true;
            } else {
                match shared
                    .socket
                    .send_to(next.packet().as_slice(), &self.key.addr)
                {
                    Ok(n) => {
                        assert_eq!(n, next.packet().as_slice().len());
                        next.sent();

                        // Reset the connection timeout
                        sent = true;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        shared.need_writable();
                        return Ok(false);
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        }

        if sent {
            self.reset_timeout();
        }

        Ok(true)
    }

    fn tick(&mut self, shared: &mut Shared, inner: &InnerCell) -> io::Result<()> {
        if self.state == State::Reset {
            return Ok(());
        }

        let now = Instant::now();
        if now > self.last_recv_time + Duration::new(u64::from(self.disconnect_timeout_secs), 0) {
            unwrap!(inner.write()).schedule_reset(self.out_queue.connection_id(), self.key.addr);
            self.state = State::Reset;
            self.update_readiness()?;
            return Ok(());
        }

        if let Some(deadline) = self.deadline {
            if now >= deadline {
                trace!(
                    "connection timed out; id={}",
                    self.out_queue.connection_id()
                );
                self.out_queue.timed_out();
                self.flush(shared)?;
            }
        }

        Ok(())
    }

    fn update_delays(&mut self, now: Instant, packet: &Packet) {
        let mut actual_delay = u32::MAX;

        if packet.timestamp() > 0 {
            // Use the packet to update the delay value
            let their_delay = self.out_queue.update_their_delay(packet.timestamp());
            let prev_base_delay = self.their_delays.base_delay();

            // Track the delay
            self.their_delays.add_sample(their_delay, now);

            if let Some(prev) = prev_base_delay {
                let new = self.their_delays.base_delay().unwrap();

                // If their new base delay is less than their previous one, we
                // should shift our delay base in the other direction in order
                // to take the clock skew into account.
                let lt = util::wrapping_lt(new, prev, TIMESTAMP_MASK);
                let diff = prev.wrapping_sub(new);

                if lt && diff <= 10_000 {
                    self.our_delays.shift(diff);
                }
            }

            actual_delay = packet.timestamp_diff();

            if actual_delay != u32::MAX {
                self.our_delays.add_sample(actual_delay, now);

                if self.average_delay_base == 0 {
                    self.average_delay_base = actual_delay;
                }

                let average_delay_sample;
                let dist_down = self.average_delay_base.wrapping_sub(actual_delay);
                let dist_up = actual_delay.wrapping_sub(self.average_delay_base);

                if dist_down > dist_up {
                    average_delay_sample = i64::from(dist_up);
                } else {
                    average_delay_sample = -i64::from(dist_down);
                }

                self.current_delay_sum = self.current_delay_sum.wrapping_add(average_delay_sample);
                self.current_delay_samples += 1;

                // recalculate delays every 5 seconds or so
                if now > self.average_sample_time {
                    let mut prev_average_delay = self.average_delay;
                    self.average_delay =
                        (self.current_delay_sum / self.current_delay_samples) as i32;
                    self.average_sample_time = now + Duration::from_secs(5);

                    self.current_delay_sum = 0;
                    self.current_delay_samples = 0;

                    let min_sample = cmp::min(prev_average_delay, self.average_delay);
                    let max_sample = cmp::max(prev_average_delay, self.average_delay);

                    if min_sample > 0 {
                        self.average_delay_base += min_sample as u32;
                        self.average_delay -= min_sample;
                        prev_average_delay -= min_sample;
                    } else if max_sample < 0 {
                        let adjust = -max_sample;

                        self.average_delay_base =
                            self.average_delay_base.saturating_sub(adjust as u32);
                        self.average_delay += adjust;
                        prev_average_delay += adjust;
                    }

                    // Update the clock drive estimate
                    let drift = i64::from(self.average_delay) - i64::from(prev_average_delay);

                    self.clock_drift = ((i64::from(self.clock_drift) * 7 + drift) / 8) as i32;
                }
            }
        }

        // Ack all packets
        if let Some((acked_bytes, min_rtt)) =
            self.out_queue
                .set_their_ack(packet.ack_nr(), &packet.selective_acks(), now)
        {
            let min_rtt = util::as_wrapping_micros(min_rtt);

            if let Some(delay) = self.our_delays.get() {
                if delay > min_rtt {
                    self.our_delays.shift(delay.wrapping_sub(min_rtt));
                }
            }

            if actual_delay != u32::MAX && acked_bytes >= 1 {
                self.apply_congestion_control(acked_bytes, actual_delay, min_rtt, now);
            }
        }
    }

    fn apply_congestion_control(
        &mut self,
        bytes_acked: usize,
        actual_delay: u32,
        min_rtt: u32,
        now: Instant,
    ) {
        trace!(
            "applying congenstion control; bytes_acked={}; actual_delay={}; min_rtt={}",
            bytes_acked,
            actual_delay,
            min_rtt
        );

        let target = TARGET_DELAY;

        let mut our_delay = cmp::min(self.our_delays.get().unwrap(), min_rtt);
        let max_window = self.out_queue.max_window() as usize;

        if self.clock_drift < -200_000 {
            let penalty = (-self.clock_drift - 200_000) / 7;

            if penalty > 0 {
                our_delay += penalty as u32;
            } else {
                our_delay -= (-penalty) as u32;
            }
        }

        let off_target = (i64::from(target) - i64::from(our_delay)) as f64;
        let window_factor =
            cmp::min(bytes_acked, max_window) as f64 / cmp::max(max_window, bytes_acked) as f64;

        let delay_factor = off_target / f64::from(target);
        let mut scaled_gain = MAX_CWND_INCREASE_BYTES_PER_RTT as f64 * window_factor * delay_factor;

        if scaled_gain > 0.0 && now - self.last_maxed_out_window > Duration::from_secs(1) {
            // if it was more than 1 second since we tried to send a packet and
            // stopped because we hit the max window, we're most likely rate
            // limited (which prevents us from ever hitting the window size) if
            // this is the case, we cannot let the max_window grow indefinitely
            scaled_gain = 0.0;
        }

        let new_window = max_window as i64 + scaled_gain as i64;
        let ledbat_cwnd = if new_window < MIN_WINDOW_SIZE as i64 {
            MIN_WINDOW_SIZE
        } else {
            new_window as usize
        };

        if self.slow_start {
            let ss_cwnd = max_window + window_factor as usize * MAX_DATA_SIZE;

            if ss_cwnd > SLOW_START_THRESHOLD || our_delay > (f64::from(target) * 0.9) as u32 {
                // Even if we're a little under the target delay, we
                // conservatively discontinue the slow start phase
                self.slow_start = false;
            } else {
                self.out_queue
                    .set_max_window(cmp::max(ss_cwnd, ledbat_cwnd) as u32);
            }
        } else {
            self.out_queue.set_max_window(ledbat_cwnd as u32);
        }
    }

    fn reset_timeout(&mut self) {
        self.deadline = self.out_queue.socket_timeout().map(|dur| {
            trace!("resetting timeout; duration={:?}", dur);
            Instant::now() + dur
        });
    }

    fn is_finalized(&self) -> bool {
        self.released
            && ((self.out_queue.is_empty() && self.state.is_closed()) || self.state == State::Reset)
    }

    /// Update the UtpStream's readiness
    fn update_readiness(&self) -> io::Result<()> {
        let mut ready = Ready::empty();

        if self.state == State::Connected {
            if self.is_readable() {
                ready.insert(Ready::readable());
            }

            if self.is_writable() {
                ready.insert(Ready::writable());
            }
        } else if self.state.is_closed() {
            ready = Ready::readable();
        }

        trace!("updating socket readiness; ready={:?}", ready);

        self.set_readiness.set_readiness(ready)
    }

    // =========

    fn is_readable(&self) -> bool {
        !self.read_open || self.in_queue.is_readable()
    }

    fn is_writable(&self) -> bool {
        self.write_open && self.out_queue.is_writable()
    }
}

impl State {
    fn is_closed(&self) -> bool {
        match *self {
            State::FinSent | State::Reset => true,
            _ => false,
        }
    }
}

impl Key {
    fn new(receive_id: u16, addr: SocketAddr) -> Key {
        Key {
            receive_id: receive_id,
            addr: addr,
        }
    }
}

/// A future that resolves with the connected `UtpStream`.
pub struct UtpStreamConnect {
    state: UtpStreamConnectState,
}

enum UtpStreamConnectState {
    Err(io::Error),
    Empty,
    Waiting(UtpStream),
}

impl Future for UtpStreamConnect {
    type Item = UtpStream;
    type Error = io::Error;

    fn poll(&mut self) -> io::Result<Async<UtpStream>> {
        let inner = mem::replace(&mut self.state, UtpStreamConnectState::Empty);
        match inner {
            UtpStreamConnectState::Waiting(stream) => match stream.registration.poll_write() {
                Async::NotReady => {
                    mem::replace(&mut self.state, UtpStreamConnectState::Waiting(stream));
                    Ok(Async::NotReady)
                }
                Async::Ready(()) => Ok(Async::Ready(stream)),
            },
            UtpStreamConnectState::Err(e) => Err(e),
            UtpStreamConnectState::Empty => panic!("can't poll UtpStreamConnect twice!"),
        }
    }
}

struct SocketRefresher {
    inner: InnerCell,
    next_tick: Instant,
    timeout: Timeout,
    req_finalize: oneshot::Receiver<oneshot::Sender<()>>,
}

impl Future for SocketRefresher {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Result<Async<()>, ()> {
        self.poll_inner().map_err(|e| {
            error!("UtpSocket died! {:?}", e);
        })
    }
}

impl SocketRefresher {
    fn poll_inner(&mut self) -> io::Result<Async<()>> {
        while let Async::Ready(()) = self.timeout.poll()? {
            unwrap!(self.inner.write()).tick(&self.inner)?;
            self.next_tick += Duration::from_millis(500);
            self.timeout.reset(self.next_tick);
        }

        if let Async::Ready(true) = unwrap!(self.inner.write()).refresh(&self.inner)? {
            if 1 == Arc::strong_count(&self.inner) {
                if let Ok(Async::Ready(resp_finalize)) = self.req_finalize.poll() {
                    let _ = resp_finalize.send(());
                }
                return Ok(Async::Ready(()));
            }
        }
        Ok(Async::NotReady)
    }
}

/// A future that resolves once the socket has been finalised. Created via `UtpSocket::finalize`.
pub struct UtpSocketFinalize {
    resp_finalize: oneshot::Receiver<()>,
}

impl Future for UtpSocketFinalize {
    type Item = ();
    type Error = Void;

    fn poll(&mut self) -> Result<Async<()>, Void> {
        if let Async::Ready(()) = unwrap!(self.resp_finalize.poll()) {
            return Ok(Async::Ready(()));
        }
        Ok(Async::NotReady)
    }
}

/// A stream of incoming uTP connections. Created via `UtpListener::incoming`.
pub struct Incoming {
    listener: UtpListener,
}

impl Stream for Incoming {
    type Item = UtpStream;
    type Error = io::Error;

    fn poll(&mut self) -> io::Result<Async<Option<UtpStream>>> {
        match self.listener.accept() {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(Async::NotReady),
            Err(e) => Err(e),
            Ok(s) => Ok(Async::Ready(Some(s))),
        }
    }
}

impl io::Read for UtpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_immutable(buf)
    }
}

impl io::Write for UtpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_immutable(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_immutable()
    }
}

impl AsyncRead for UtpStream {}

impl AsyncWrite for UtpStream {
    fn shutdown(&mut self) -> io::Result<Async<()>> {
        self.shutdown_write()?;
        Ok(Async::Ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod inner {
        use super::*;
        use futures::future::{self, Loop};
        use futures::sync::mpsc;
        use tokio_core::reactor::Core;

        /// Creates new UDP socket and waits for incoming uTP packets.
        /// Returns future that yields received packet and listener address.
        fn wait_for_packets(handle: &Handle) -> (mpsc::UnboundedReceiver<Packet>, SocketAddr) {
            let (packets_tx, packets_rx) = mpsc::unbounded();
            let listener = unwrap!(UdpSocket::bind(&addr!("127.0.0.1:0"), handle));
            let listener_addr = unwrap!(listener.local_addr());

            let recv_response =
                future::loop_fn((listener, packets_tx), |(listener, packets_tx)| {
                    listener.recv_dgram(vec![0u8; 256]).map(
                        move |(listener, buff, bytes_received, _addr)| {
                            let buff = BytesMut::from(&buff[..bytes_received]);
                            let packet = unwrap!(Packet::parse(buff));
                            unwrap!(packets_tx.unbounded_send(packet));
                            Loop::Continue((listener, packets_tx))
                        },
                    )
                }).map_err(|e| panic!(e))
                    .then(|_res: Result<(), _>| Ok(()));
            handle.spawn(recv_response);

            (packets_rx, listener_addr)
        }

        mod process_unknown {
            use super::*;

            #[test]
            fn when_packet_is_syn_it_schedules_reset_back() {
                let evloop = unwrap!(Core::new());
                let handle = evloop.handle();
                let (_listener_registration, listener_set_readiness) = Registration::new2();

                let socket = unwrap!(UdpSocket::bind(&addr!("127.0.0.1:0"), &handle));
                let inner = Inner::new_shared(&handle, socket, listener_set_readiness);

                let mut packet = Packet::syn();
                packet.set_connection_id(12_345);
                let peer_addr = addr!("1.2.3.4:5000");

                unwrap!(unwrap!(inner.write()).process_unknown(packet, peer_addr, &inner));

                let packet_opt = unwrap!(inner.write()).reset_packets.pop_front();
                let (packet, dest_addr) = unwrap!(packet_opt);
                assert_eq!(packet.connection_id(), 12_345);
                assert_eq!(packet.ty(), packet::Type::Reset);
                assert_eq!(dest_addr, addr!("1.2.3.4:5000"));
            }

            #[test]
            fn when_packet_is_reset_nothing_is_sent_back() {
                let evloop = unwrap!(Core::new());
                let handle = evloop.handle();
                let (_listener_registration, listener_set_readiness) = Registration::new2();

                let socket = unwrap!(UdpSocket::bind(&addr!("127.0.0.1:0"), &handle));
                let inner = Inner::new_shared(&handle, socket, listener_set_readiness);

                let mut packet = Packet::reset();
                packet.set_connection_id(12_345);
                let peer_addr = addr!("1.2.3.4:5000");

                unwrap!(unwrap!(inner.write()).process_unknown(packet, peer_addr, &inner));

                assert!(unwrap!(inner.write()).reset_packets.is_empty());
            }
        }

        mod flush_reset_packets {
            use super::*;
            use hamcrest::prelude::*;

            #[test]
            fn it_attempts_to_send_all_queued_reset_packets() {
                let mut evloop = unwrap!(Core::new());
                let handle = evloop.handle();

                let task = future::lazy(|| {
                    let (_listener_registration, listener_set_readiness) = Registration::new2();
                    let socket = unwrap!(UdpSocket::bind(&addr!("127.0.0.1:0"), &handle));
                    let inner = Inner::new_shared(&handle, socket, listener_set_readiness);

                    let (packets_rx, remote_peer_addr) = wait_for_packets(&handle);

                    let mut packet = Packet::reset();
                    packet.set_connection_id(12_345);
                    unwrap!(
                        unwrap!(inner.write())
                            .reset_packets
                            .push_back((packet, remote_peer_addr))
                    );
                    let mut packet = Packet::reset();
                    packet.set_connection_id(23_456);
                    unwrap!(
                        unwrap!(inner.write())
                            .reset_packets
                            .push_back((packet, remote_peer_addr))
                    );

                    // keep retrying until all packets are sent out
                    future::poll_fn(move || unwrap!(inner.write()).flush_reset_packets())
                        .and_then(move |_| packets_rx.take(2).collect().map_err(|e| panic!(e)))
                });
                let packets = unwrap!(evloop.run(task));

                let packet_ids: Vec<u16> = packets.iter().map(|p| p.connection_id()).collect();
                assert_that!(&packet_ids, contains(vec![12_345, 23_456]).exactly());
            }
        }

        mod process_syn {
            use super::*;

            #[test]
            fn when_listener_is_closed_it_schedules_reset_packet_back() {
                let evloop = unwrap!(Core::new());
                let handle = evloop.handle();
                let (_listener_registration, listener_set_readiness) = Registration::new2();

                let socket = unwrap!(UdpSocket::bind(&addr!("127.0.0.1:0"), &handle));
                let inner = Inner::new_shared(&handle, socket, listener_set_readiness);
                unwrap!(inner.write()).listener_open = false;

                let mut packet = Packet::syn();
                packet.set_connection_id(12_345);
                let peer_addr = addr!("1.2.3.4:5000");

                unwrap!(unwrap!(inner.write()).process_syn(&packet, peer_addr, &inner));

                let packet_opt = unwrap!(inner.write()).reset_packets.pop_front();
                let (packet, dest_addr) = unwrap!(packet_opt);
                assert_eq!(packet.connection_id(), 12_345);
                assert_eq!(packet.ty(), packet::Type::Reset);
                assert_eq!(dest_addr, addr!("1.2.3.4:5000"));
            }
        }
    }
}
