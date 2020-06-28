pub mod protocol {
    use log::*;

    use crate::error::Error;
    use crate::peer::{Config, Link};
    use crate::PeerId;

    use std::collections::{HashMap, HashSet};
    use std::fmt::Debug;
    use std::net;
    use std::time::{self, SystemTime, UNIX_EPOCH};

    use bitcoin::network::address::Address;
    use bitcoin::network::constants::ServiceFlags;
    use bitcoin::network::message::{NetworkMessage, RawNetworkMessage};
    use bitcoin::network::message_network::VersionMessage;
    use bitcoin::util::hash::BitcoinHash;

    use nakamoto_chain::block::time::AdjustedTime;
    use nakamoto_chain::block::tree::BlockTree;
    use nakamoto_chain::block::{BlockHash, BlockHeader, Height};

    /// User agent included in `version` messages.
    pub const USER_AGENT: &str = "/nakamoto:0.0.0/";
    /// Duration of inactivity before timing out a peer.
    pub const IDLE_TIMEOUT: time::Duration = time::Duration::from_secs(60 * 5);
    /// How long to wait between sending pings.
    pub const PING_INTERVAL: time::Duration = time::Duration::from_secs(60);
    /// Number of blocks out of sync we have to be to trigger an initial sync.
    pub const SYNC_THRESHOLD: Height = 144;
    /// Minimum number of peers to be connected to.
    pub const PEER_CONNECTION_THRESHOLD: usize = 3;
    /// Maximum time adjustment between network and local time (70 minutes).
    pub const MAX_TIME_ADJUSTMENT: TimeOffset = 70 * 60;

    /// A time offset, in seconds.
    pub type TimeOffset = i64;

    #[derive(Debug)]
    pub enum Event<T> {
        Connected(net::SocketAddr, net::SocketAddr, Link),
        Received(net::SocketAddr, T),
        Sent(net::SocketAddr, usize),
        Error(net::SocketAddr, Error),
    }

    pub trait Protocol<M> {
        /// Process the next event and advance the protocol state-machine by one step.
        fn step(&mut self, event: Event<M>) -> Vec<(net::SocketAddr, M)>;
    }

    ///////////////////////////////////////////////////////////////////////////////////////////////

    #[derive(Debug)]
    pub enum State {
        /// Connecting to the network. Syncing hasn't started yet.
        Connecting,
        /// Initial syncing (IBD) has started with the designated peer.
        InitialSync(PeerId),
        /// We're in sync.
        Synced,
    }

    #[derive(Debug)]
    pub struct Rpc<T> {
        /// Peers.
        pub peers: HashMap<PeerId, Peer>,
        /// Peer configuration.
        pub config: Config,
        /// Protocol state machine.
        pub state: State,
        /// Block tree.
        pub tree: T,

        /// Network-adjusted clock.
        clock: AdjustedTime<PeerId>,
        /// Set of connected peers that have completed the handshake.
        connected: HashSet<PeerId>,
        /// Set of disconnected peers.
        disconnected: HashSet<PeerId>,
    }

    impl<T: BlockTree> Rpc<T> {
        pub fn new(tree: T, clock: AdjustedTime<net::SocketAddr>, config: Config) -> Self {
            Self {
                peers: HashMap::new(),
                config,
                state: State::Connecting,
                tree,
                clock,
                connected: HashSet::new(),
                disconnected: HashSet::new(),
            }
        }

        fn connect(&mut self, addr: PeerId, local_addr: net::SocketAddr, link: Link) -> bool {
            self.disconnected.remove(&addr);

            match self.peers.insert(
                addr,
                Peer::new(
                    addr,
                    local_addr,
                    PeerState::Handshake(Handshake::default()),
                    link,
                ),
            ) {
                Some(_) => false,
                None => true,
            }
        }

        /// Start initial block header sync.
        pub fn initial_sync(&mut self, peer: PeerId) {
            // TODO: Notify peer that it should sync.
            self.state = State::InitialSync(peer);
        }

        /// Check whether or not we are in sync with the network.
        pub fn is_synced(&self) -> Result<bool, Error> {
            let height = self.tree.height();

            // TODO: Make sure we only consider connected peers?
            // TODO: Check actual block hashes once we are caught up on height.
            if let Some(peer_height) = self.peers.values().map(|p| p.height).min() {
                Ok(height >= peer_height || peer_height - height <= SYNC_THRESHOLD)
            } else {
                Err(Error::NotConnected)
            }
        }
    }

    /// Handshake states.
    ///
    /// The steps for an *outbound* handshake are:
    ///
    ///   1. Send "version" message.
    ///   2. Expect "version" message from remote.
    ///   3. Expect "verack" message from remote.
    ///   4. Send "verack" message.
    ///
    /// The steps for an *inbound* handshake are:
    ///
    ///   1. Expect "version" message from remote.
    ///   2. Send "version" message.
    ///   3. Send "verack" message.
    ///   4. Expect "verack" message from remote.
    ///
    #[derive(Copy, Clone, Debug, PartialOrd, PartialEq, Ord, Eq)]
    pub enum Handshake {
        /// Waiting for "version" message from remote.
        AwaitingVersion,
        /// Waiting for "verack" message from remote.
        AwaitingVerack,
        /// The peer handshake was completed.
        Done,
    }

    impl Default for Handshake {
        fn default() -> Self {
            Self::AwaitingVersion
        }
    }

    #[derive(Copy, Clone, Debug)]
    pub enum Synchronize {
        // TODO: This should keep track of what height we're at.
        RequestedHeaders,
        HeadersReceived,
        Done,
    }

    impl Default for Synchronize {
        fn default() -> Self {
            Self::RequestedHeaders
        }
    }

    #[derive(Debug)]
    pub enum PeerState {
        Handshake(Handshake),
        Synchronize(Synchronize),
    }

    #[derive(Debug)]
    pub struct Peer {
        /// Remote peer address.
        pub address: net::SocketAddr,
        /// Local peer address.
        pub local_address: net::SocketAddr,
        /// The peer's best height.
        pub height: Height,
        /// An offset in seconds, between this peer's clock and ours.
        /// A positive offset means the peer's clock is ahead of ours.
        pub time_offset: TimeOffset,
        /// Whether this is an inbound or outbound peer connection.
        pub link: Link,
        /// Peer state.
        pub state: PeerState,
        /// Last time we heard from this peer.
        pub last_active: Option<time::Instant>,
    }

    impl Peer {
        pub fn new(
            address: net::SocketAddr,
            local_address: net::SocketAddr,
            state: PeerState,
            link: Link,
        ) -> Self {
            Self {
                address,
                local_address,
                height: 0,
                time_offset: 0,
                last_active: None,
                state,
                link,
            }
        }

        fn receive_version(
            &mut self,
            VersionMessage {
                start_height,
                timestamp,
                ..
            }: VersionMessage,
        ) {
            let height = 0;
            // TODO: I'm not sure we should be getting the system time here.
            // It may be a better idea _not_ to store the time offset locally,
            // and instead send the remote time back to the network controller.
            let local_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;

            if height > start_height as Height + 1 {
                // We're ahead of this peer by more than one block. Don't use it
                // for IBD.
                todo!();
            }
            // TODO: Check version
            // TODO: Check services
            // TODO: Check start_height
            self.height = start_height as Height;
            self.time_offset = timestamp - local_time;

            self.transition(PeerState::Handshake(Handshake::AwaitingVerack));
        }

        fn receive_verack(&mut self) {
            self.transition(PeerState::Handshake(Handshake::Done));
        }

        fn transition(&mut self, state: PeerState) {
            debug!("{}: {:?} -> {:?}", self.address, self.state, state);

            self.state = state;
        }
    }

    impl<T: BlockTree> Protocol<RawNetworkMessage> for Rpc<T> {
        fn step(
            &mut self,
            event: Event<RawNetworkMessage>,
        ) -> Vec<(net::SocketAddr, RawNetworkMessage)> {
            let outbound = match event {
                Event::Connected(addr, local_addr, link) => {
                    self.connect(addr, local_addr, link);

                    match link {
                        Link::Outbound => vec![(addr, self.version(addr, local_addr, 0))],
                        Link::Inbound => vec![],
                    }
                }
                Event::Received(addr, msg) => self.receive(addr, msg),
                Event::Sent(_addr, _msg) => vec![],
                Event::Error(addr, err) => {
                    debug!("Disconnected from {}", &addr);
                    debug!("error: {}: {}", addr, err);

                    self.connected.remove(&addr);
                    self.disconnected.insert(addr);
                    // TODO: Protocol shouldn't handle socket and io errors directly, because it
                    // needs to understand all kinds of socket errors then, even though it's agnostic
                    // to the transport. This doesn't make sense. What should happen is that
                    // transport errors should be handled at the transport (or reactor) layer. The "protocol"
                    // doesn't decide on what to do about transport errors. It _may_ receive a higher
                    // level event like `Disconnected`, or an opaque `Error`, just to keep track of
                    // peer errors, scores etc.
                    // TODO: If this is a disconnect, then we need to send Command::Quit to the
                    // connection somehow. Maybe not directly here, but perhaps this should return
                    // not just messages but also the ability to drop a peer?
                    // TODO: The other option is that error events (Event::Error) and disconnects
                    // are handled one layer above. But this means the protocol can't decide on
                    // these things, but instead it is the reactor that does.
                    vec![]
                }
            };

            if self.connected.len() >= PEER_CONNECTION_THRESHOLD {
                match self.is_synced() {
                    Ok(is_synced) => {
                        if is_synced {
                            self.state = State::Synced;
                        } else {
                            let ix = fastrand::usize(..self.connected.len());
                            let peer = *self.connected.iter().nth(ix).unwrap();

                            self.initial_sync(peer);
                        }
                    }
                    Err(Error::NotConnected) => self.state = State::Connecting,
                    Err(err) => panic!(err.to_string()),
                }
            }

            outbound
                .into_iter()
                .map(|(addr, msg)| {
                    (
                        addr,
                        RawNetworkMessage {
                            magic: self.config.network.magic(),
                            payload: msg,
                        },
                    )
                })
                .collect()
        }
    }

    impl<T: BlockTree> Rpc<T> {
        pub fn receive(
            &mut self,
            addr: net::SocketAddr,
            msg: RawNetworkMessage,
        ) -> Vec<(net::SocketAddr, NetworkMessage)> {
            debug!("{}: Received {:?}", addr, msg.cmd());

            if msg.magic != self.config.network.magic() {
                // TODO: Send rejection messsage to peer and close connection.
                todo!();
            }
            let peer = self
                .peers
                .get_mut(&addr)
                .unwrap_or_else(|| panic!("peer {} is not known", addr));
            let local_addr = peer.local_address;

            peer.last_active = Some(time::Instant::now());

            match peer.state {
                PeerState::Handshake(Handshake::AwaitingVersion) => {
                    if let NetworkMessage::Version(version) = msg.payload {
                        peer.receive_version(version);

                        match peer.link {
                            Link::Outbound => {}
                            Link::Inbound => {
                                return vec![
                                    (addr, self.version(addr, local_addr, 0)),
                                    (addr, NetworkMessage::Verack),
                                ]
                            }
                        }
                    }
                }
                PeerState::Handshake(Handshake::AwaitingVerack) => {
                    if msg.payload == NetworkMessage::Verack {
                        peer.receive_verack();

                        self.connected.insert(addr);
                        self.clock.add_sample(addr, peer.time_offset);

                        if peer.link == Link::Outbound {
                            return vec![(addr, NetworkMessage::Verack)];
                        }
                    }
                }
                PeerState::Handshake(Handshake::Done) => {
                    peer.state = PeerState::Synchronize(Synchronize::default());
                }
                PeerState::Synchronize(_) => {}
            }

            vec![]
        }

        pub fn transition(&mut self, addr: net::SocketAddr, state: State) {
            debug!("{}: {:?} -> {:?}", addr, self.state, state);

            self.state = state;
        }

        fn _receive_headers(
            &mut self,
            addr: net::SocketAddr,
            headers: Vec<BlockHeader>,
        ) -> Result<Option<(BlockHash, Height)>, Error> {
            debug!("{}: Received {} headers", addr, headers.len());

            if let (Some(first), Some(last)) = (headers.first(), headers.last()) {
                debug!(
                    "{}: Range = {}..{}",
                    addr,
                    first.bitcoin_hash(),
                    last.bitcoin_hash()
                );
            } else {
                info!("{}: Finished synchronizing", addr);
                return Ok(None);
            }

            let length = headers.len();

            match self.tree.import_blocks(headers.into_iter()) {
                Ok((tip, height)) => {
                    let peer = self.peers.get_mut(&addr).unwrap();
                    peer.height = height;

                    info!("Imported {} headers from {}", length, addr);
                    info!("Chain height = {}, tip = {}", height, tip);
                    // TODO: We can break here if we've received less than 2'000 headers.
                    Ok(Some((tip, height)))
                }
                Err(err) => {
                    error!("Error importing headers: {}", err);
                    return Err(Error::from(err));
                }
            }
        }

        fn version(
            &self,
            addr: net::SocketAddr,
            local_addr: net::SocketAddr,
            start_height: Height,
        ) -> NetworkMessage {
            let start_height = start_height as i32;
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;

            NetworkMessage::Version(VersionMessage {
                version: self.config.protocol_version,
                services: self.config.services,
                timestamp,
                receiver: Address::new(
                    &addr,
                    ServiceFlags::NETWORK | ServiceFlags::COMPACT_FILTERS,
                ),
                sender: Address::new(&local_addr, ServiceFlags::NONE),
                nonce: 0,
                user_agent: USER_AGENT.to_owned(),
                start_height,
                relay: self.config.relay,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use nakamoto_chain::block::cache::model;
        use std::collections::VecDeque;

        mod simulator {
            use super::*;

            pub fn run<P: Protocol<M>, M>(peers: Vec<(PeerId, &mut P, Vec<Event<M>>)>) {
                let mut sim: HashMap<PeerId, (&mut P, VecDeque<Event<M>>)> = HashMap::new();
                let mut events = Vec::new();

                // Add peers to simulator.
                for (addr, proto, evs) in peers.into_iter() {
                    sim.insert(addr, (proto, VecDeque::new()));

                    for e in evs.into_iter() {
                        events.push((addr, e));
                    }
                }

                while !events.is_empty() || sim.values().any(|(_, q)| !q.is_empty()) {
                    // Prepare event queues.
                    for (receiver, event) in events.drain(..) {
                        let (_, q) = sim.get_mut(&receiver).unwrap();
                        q.push_back(event);
                    }

                    for (peer, (proto, queue)) in sim.iter_mut() {
                        if let Some(event) = queue.pop_front() {
                            let out = proto.step(event);

                            for (receiver, msg) in out.into_iter() {
                                events.push((receiver, Event::Received(*peer, msg)));
                            }
                        }
                    }
                }
            }
        }

        #[test]
        fn test_handshake() {
            let genesis = BlockHeader {
                version: 1,
                prev_blockhash: Default::default(),
                merkle_root: Default::default(),
                nonce: 0,
                time: 0,
                bits: 0,
            };
            let tree = model::Cache::new(genesis);
            let config = Config::default();
            let clock = AdjustedTime::new();

            let alice_addr = ([127, 0, 0, 1], 8333).into();
            let bob_addr = ([127, 0, 0, 2], 8333).into();

            let mut alice = Rpc::new(tree.clone(), clock.clone(), config);
            let mut bob = Rpc::new(tree, clock, config);

            fern::Dispatch::new()
                .format(move |out, message, record| {
                    out.finish(format_args!(
                        "{:5} [{}] {}",
                        record.level(),
                        record.target(),
                        message
                    ))
                })
                .level(log::LevelFilter::Debug)
                .chain(std::io::stderr())
                .apply()
                .unwrap();

            simulator::run(vec![
                (
                    alice_addr,
                    &mut alice,
                    vec![Event::Connected(bob_addr, alice_addr, Link::Outbound)],
                ),
                (
                    bob_addr,
                    &mut bob,
                    vec![Event::Connected(alice_addr, bob_addr, Link::Inbound)],
                ),
            ]);

            assert!(
                alice
                    .peers
                    .values()
                    .all(|p| matches!(p.state, PeerState::Handshake(Handshake::Done))),
                "alice: {:#?}",
                alice.peers
            );

            assert!(
                bob.peers
                    .values()
                    .all(|p| matches!(p.state, PeerState::Handshake(Handshake::Done))),
                "bob: {:#?}",
                bob.peers
            );
        }
    }
}

///////////////////////////////////////////////////////////////////////////////////////////////////

pub mod reactor {
    use bitcoin::consensus::encode::Decodable;
    use bitcoin::consensus::encode::{self, Encodable};
    use bitcoin::network::stream_reader::StreamReader;

    use super::protocol::{Event, Protocol, IDLE_TIMEOUT, PING_INTERVAL};

    use crate::address_book::AddressBook;
    use crate::error::Error;
    use crate::peer::Link;

    use log::*;
    use std::collections::HashMap;
    use std::fmt::Debug;
    use std::io::prelude::*;
    use std::net;

    use crossbeam_channel as crossbeam;

    /// Stack size for spawned threads, in bytes.
    /// Since we're creating a thread per peer, we want to keep the stack size small.
    const THREAD_STACK_SIZE: usize = 1024 * 1024;

    /// Maximum peer-to-peer message size.
    pub const MAX_MESSAGE_SIZE: usize = 6 * 1024;

    #[derive(Debug)]
    pub enum Command<T> {
        Write(net::SocketAddr, T),
        Disconnect(net::SocketAddr),
        Quit,
    }

    #[derive(Debug)]
    pub struct Reader<R: Read + Write, M> {
        events: crossbeam::Sender<Event<M>>,
        raw: StreamReader<R>,
        address: net::SocketAddr,
        local_address: net::SocketAddr,
    }

    impl<R: Read + Write, M: Decodable + Encodable + Debug + Send + Sync + 'static> Reader<R, M> {
        /// Create a new peer from a `io::Read` and an address pair.
        pub fn from(
            r: R,
            local_address: net::SocketAddr,
            address: net::SocketAddr,
            events: crossbeam::Sender<Event<M>>,
        ) -> Self {
            let raw = StreamReader::new(r, Some(MAX_MESSAGE_SIZE));

            Self {
                raw,
                local_address,
                address,
                events,
            }
        }

        pub fn run(&mut self, link: Link) -> Result<(), Error> {
            self.events
                .send(Event::Connected(self.address, self.local_address, link))?;

            loop {
                match self.read() {
                    Ok(msg) => self.events.send(Event::Received(self.address, msg))?,
                    Err(err) => {
                        self.events.send(Event::Error(self.address, err.into()))?;
                        break;
                    }
                }
            }
            Ok(())
        }

        pub fn read(&mut self) -> Result<M, encode::Error> {
            match self.raw.read_next::<M>() {
                Ok(msg) => {
                    trace!("{}: {:#?}", self.address, msg);

                    Ok(msg)
                }
                Err(err) => Err(err),
            }
        }
    }

    ///////////////////////////////////////////////////////////////////////////////////////////////

    pub struct Writer<T> {
        address: net::SocketAddr,
        raw: T,
    }

    impl<T: Write> Writer<T> {
        pub fn write<M: Encodable + Debug>(&mut self, msg: M) -> Result<usize, Error> {
            let mut buf = [0u8; MAX_MESSAGE_SIZE];

            match msg.consensus_encode(&mut buf[..]) {
                Ok(len) => {
                    trace!("{}: {:#?}", self.address, msg);

                    self.raw.write_all(&buf[..len])?;
                    self.raw.flush()?;

                    Ok(len)
                }
                Err(err) => panic!(err.to_string()),
            }
        }

        fn thread<M: Encodable + Send + Sync + Debug + 'static>(
            mut peers: HashMap<net::SocketAddr, Self>,
            cmds: crossbeam::Receiver<Command<M>>,
            events: crossbeam::Sender<Event<M>>,
        ) -> Result<(), Error> {
            loop {
                let cmd = cmds.recv().unwrap();

                match cmd {
                    Command::Write(addr, msg) => {
                        let peer = peers.get_mut(&addr).unwrap();

                        match peer.write(msg) {
                            Ok(nbytes) => {
                                events.send(Event::Sent(addr, nbytes))?;
                            }
                            Err(err) => {
                                events.send(Event::Error(addr, err))?;
                            }
                        }
                    }
                    Command::Disconnect(addr) => {
                        peers.remove(&addr);
                    }
                    Command::Quit => break,
                }
            }
            Ok(())
        }
    }

    impl<T: Write> std::ops::Deref for Writer<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.raw
        }
    }

    impl<T: Write> std::ops::DerefMut for Writer<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.raw
        }
    }

    pub fn run<P: Protocol<M>, M: Decodable + Encodable + Send + Sync + Debug + 'static>(
        addrs: AddressBook,
        mut protocol: P,
    ) -> Result<Vec<()>, Error> {
        use std::thread;

        let (events_tx, events_rx): (crossbeam::Sender<Event<M>>, _) = crossbeam::bounded(1);
        let (cmds_tx, cmds_rx) = crossbeam::bounded(1);

        let mut spawned = Vec::with_capacity(addrs.len());
        let mut peers = HashMap::new();

        for addr in addrs.iter() {
            let (mut conn, writer) = self::dial(&addr, events_tx.clone())?;

            debug!("Connected to {}", &addr);
            trace!("{:#?}", conn);

            peers.insert(*addr, writer);

            let handle = thread::Builder::new()
                .name(addr.to_string())
                .stack_size(THREAD_STACK_SIZE)
                .spawn(move || conn.run(Link::Outbound))?;

            spawned.push(handle);
        }

        thread::Builder::new().spawn(move || Writer::thread(peers, cmds_rx, events_tx))?;

        loop {
            let result = events_rx.recv_timeout(PING_INTERVAL);

            match result {
                Ok(event) => {
                    let msgs = protocol.step(event);

                    for (addr, msg) in msgs.into_iter() {
                        cmds_tx.send(Command::Write(addr, msg)).unwrap();
                    }
                }
                Err(crossbeam::RecvTimeoutError::Disconnected) => {
                    // TODO: We need to connect to new peers.
                    // This always means that all senders have been dropped.
                    break;
                }
                Err(crossbeam::RecvTimeoutError::Timeout) => {
                    // TODO: Ping peers, nothing was received in a while. Find out
                    // who to ping.
                }
            }
        }

        spawned
            .into_iter()
            .flat_map(thread::JoinHandle::join)
            .collect()
    }

    /// Connect to a peer given a remote address.
    pub fn dial<M: Encodable + Decodable + Send + Sync + Debug + 'static>(
        addr: &net::SocketAddr,
        events_tx: crossbeam::Sender<Event<M>>,
    ) -> Result<(Reader<net::TcpStream, M>, Writer<net::TcpStream>), Error> {
        debug!("Connecting to {}...", &addr);

        let sock = net::TcpStream::connect(addr)?;

        sock.set_read_timeout(Some(IDLE_TIMEOUT))?;
        sock.set_write_timeout(Some(IDLE_TIMEOUT))?;

        let w = sock.try_clone()?;
        let address = sock.peer_addr()?;
        let local_address = sock.local_addr()?;

        Ok((
            Reader::from(sock, local_address, address, events_tx),
            Writer { raw: w, address },
        ))
    }
}
