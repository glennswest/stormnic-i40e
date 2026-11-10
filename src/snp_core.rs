//! The `EFI_SIMPLE_NETWORK_PROTOCOL` state machine and data path, from the
//! UEFI specification's Simple Network Protocol section, over `ring` and
//! `adminq`. Taken from stormnic-igb's.
//!
//! Independent of UEFI like the rest of `i40e`, so every call can be tested
//! against the simulated device: `src/snp.rs` turns firmware calls into these
//! methods and `Fail` into an EFI status.
//!
//! The device's switch passes the station address (the firmware's own MAC
//! filter, and the one Start adds for any VLAN); the VSI's promiscuous modes
//! (one admin command) pass a superset of the rest: broadcast when asked,
//! every multicast when any multicast is asked, everything when promiscuous.
//! `accepts` drops in software what the SNP setting does not allow. The
//! station address is the firmware's: Mode says it is not changeable, and
//! StationAddress takes only it.
//!
//! Link changes come as events on the admin receive queue (asked for at
//! Initialize), read at every GetStatus.
//!
//! The queues run only between Initialize and Shutdown/Stop (or halt, at
//! ExitBootServices): every other state has them disabled.
//!
//! Transmit copies the frame into a ring buffer, so the caller's buffer is
//! free at once; it is queued for GetStatus to hand back, as the spec asks.

use super::adminq::AdminQ;
use super::ring::{Net, MAX_FRAME, MIN_FRAME};
use super::{Bus, Error, Link};

/// Ethernet: header size, largest payload.
pub const HEADER: usize = 14;
pub const MAX_PACKET: usize = 1500;
/// Multicast addresses ReceiveFilters takes (Mode.MCastFilter has 16 slots).
pub const MAX_MCAST: usize = 16;
/// Transmitted buffers waiting for GetStatus; Transmit is NOT_READY when full.
pub const RECYCLE: usize = 64;

/// `EFI_SIMPLE_NETWORK_RECEIVE_*` filter bits.
pub const UNICAST: u32 = 0x01;
pub const MULTICAST: u32 = 0x02;
pub const BROADCAST: u32 = 0x04;
pub const PROMISCUOUS: u32 = 0x08;
pub const PROMISCUOUS_MULTICAST: u32 = 0x10;
pub const FILTER_MASK: u32 = 0x1f;
/// `EFI_SIMPLE_NETWORK_*_INTERRUPT` bits for GetStatus.
pub const RECEIVE_INTERRUPT: u32 = 0x01;
pub const TRANSMIT_INTERRUPT: u32 = 0x02;

const BROADCAST_ADDRESS: [u8; 6] = [0xff; 6];

/// `EFI_SIMPLE_NETWORK_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State { Stopped, Started, Initialized }

/// Why a call failed; `src/snp.rs` maps each to the spec's status.
#[derive(Debug, PartialEq, Eq)]
pub enum Fail<E> {
    /// EFI_NOT_STARTED: the interface is stopped.
    NotStarted,
    /// EFI_ALREADY_STARTED.
    AlreadyStarted,
    /// EFI_DEVICE_ERROR: started but not initialized.
    NotInitialized,
    InvalidParameter,
    /// EFI_UNSUPPORTED: a station address other than the NVM's.
    Unsupported,
    /// EFI_BUFFER_TOO_SMALL; for Receive, the frame's length (it stays queued).
    BufferTooSmall(usize),
    /// EFI_NOT_READY: nothing received, or no room to transmit.
    NotReady,
    /// EFI_DEVICE_ERROR: the device failed.
    Device(Error<E>),
}

impl<E> From<Error<E>> for Fail<E> {
    fn from(e: Error<E>) -> Self { Fail::Device(e) }
}

type F<T, E> = Result<T, Fail<E>>;

/// A received frame's header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    pub len: usize,
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub protocol: u16,
}

pub struct Snp {
    pub net: Net,
    pub aq: AdminQ,
    pub state: State,
    pub permanent: [u8; 6],
    pub current: [u8; 6],
    /// Enabled `*_RECEIVE_*` bits.
    pub setting: u32,
    pub mcast: [[u8; 6]; MAX_MCAST],
    pub mcast_count: usize,
    pub media: bool,
    /// The last link state read.
    pub link: Link,
    /// A frame went out since the last GetStatus.
    transmitted: bool,
    /// Caller buffers (as addresses) for GetStatus, oldest at `head`.
    recycle: [usize; RECYCLE],
    head: usize,
    queued: usize,
}

/// `EFI_SIMPLE_NETWORK_PROTOCOL.MCastIpToMac`: IPv4 224.0.0.0/4 maps to
/// 01:00:5e plus the low 23 bits (RFC 1112); IPv6 ff00::/8 to 33:33 plus the
/// low 32 bits (RFC 2464).
pub fn mcast_ip_to_mac(ipv6: bool, ip: &[u8; 16]) -> Option<[u8; 6]> {
    if ipv6 {
        (ip[0] == 0xff).then(|| [0x33, 0x33, ip[12], ip[13], ip[14], ip[15]])
    } else {
        (ip[0] & 0xf0 == 0xe0).then(|| [0x01, 0x00, 0x5e, ip[1] & 0x7f, ip[2], ip[3]])
    }
}

fn is_multicast(mac: [u8; 6]) -> bool { mac[0] & 1 != 0 }

impl Snp {
    pub fn new(net: Net, aq: AdminQ, link: Link) -> Self {
        let mac = net.mac;
        Snp {
            net, aq, state: State::Stopped, permanent: mac, current: mac,
            setting: 0, mcast: [[0; 6]; MAX_MCAST], mcast_count: 0, media: link.up, link,
            transmitted: false, recycle: [0; RECYCLE], head: 0, queued: 0,
        }
    }

    fn initialized<E>(&self) -> F<(), E> {
        match self.state {
            State::Stopped => Err(Fail::NotStarted),
            State::Started => Err(Fail::NotInitialized),
            State::Initialized => Ok(()),
        }
    }

    /// Start: stopped to started.
    pub fn start<E>(&mut self) -> F<(), E> {
        if self.state != State::Stopped { return Err(Fail::AlreadyStarted); }
        self.state = State::Started;
        Ok(())
    }

    /// Stop: started to stopped. An initialized interface is shut down
    /// first, so the NIC never runs on a stopped one.
    pub fn stop<B: Bus>(&mut self, b: &mut B) -> F<(), B::Error> {
        match self.state {
            State::Stopped => return Err(Fail::NotStarted),
            State::Initialized => self.shutdown(b)?,
            State::Started => {}
        }
        self.state = State::Stopped;
        Ok(())
    }

    /// Initialize: bring the NIC up with every receive filter off (the
    /// caller then sets them); an initialized interface is re-initialized.
    /// The extra buffer sizes of the spec call are not needed: buffers are
    /// the driver's own.
    pub fn initialize<B: Bus>(&mut self, b: &mut B) -> F<(), B::Error> {
        match self.state {
            State::Stopped => return Err(Fail::NotStarted),
            State::Initialized => { self.net.stop(b)?; }
            State::Started => {}
        }
        self.setting = 0;
        self.mcast_count = 0;
        self.run(b)?;
        self.state = State::Initialized;
        Ok(())
    }

    /// Reset: restart the NIC, keeping the filters and station address.
    pub fn reset<B: Bus>(&mut self, b: &mut B) -> F<(), B::Error> {
        self.initialized()?;
        self.net.stop(b)?;
        self.run(b)
    }

    /// Shutdown: quiesce the NIC; initialized to started.
    pub fn shutdown<B: Bus>(&mut self, b: &mut B) -> F<(), B::Error> {
        self.initialized()?;
        self.net.stop(b)?;
        self.state = State::Started;
        Ok(())
    }

    /// Start the queues, set the switch's filters for the current setting,
    /// and read the link with change events turned on.
    fn run<B: Bus>(&mut self, b: &mut B) -> F<(), B::Error> {
        if let Err(e) = self.net.start(b) {
            // `start` disabled the queues again; nothing runs.
            self.state = State::Started;
            return Err(e.into());
        }
        let r = self.write_filter(b, self.setting).and_then(|()| self.aq.link(b, true));
        match r {
            Ok(link) => {
                self.link = link;
                self.media = link.up;
                Ok(())
            }
            Err(e) => {
                let _ = self.net.stop(b);
                self.state = State::Started;
                Err(e.into())
            }
        }
    }

    /// The VSI's promiscuous modes for `setting`: a superset of it.
    fn write_filter<B: Bus>(&mut self, b: &mut B, setting: u32) -> Result<(), Error<B::Error>> {
        let promiscuous = setting & PROMISCUOUS != 0;
        let multicast = promiscuous || setting & (MULTICAST | PROMISCUOUS_MULTICAST) != 0;
        let broadcast = promiscuous || setting & BROADCAST != 0;
        self.aq.promiscuous(b, self.net.vsi.seid, promiscuous, multicast, broadcast)
    }

    /// ReceiveFilters: new setting = (setting | enable) & !disable. With
    /// `reset_mcast` the list is emptied; otherwise a non-empty `list`
    /// replaces it (multicast addresses only, at most 16, MULTICAST on).
    pub fn receive_filters<B: Bus>(&mut self, b: &mut B, enable: u32, disable: u32, reset_mcast: bool,
        list: &[[u8; 6]]) -> F<(), B::Error> {
        self.initialized()?;
        if (enable | disable) & !FILTER_MASK != 0 { return Err(Fail::InvalidParameter); }
        let setting = (self.setting | enable) & !disable;
        if !reset_mcast && !list.is_empty() {
            if list.len() > MAX_MCAST || setting & MULTICAST == 0
                || list.iter().any(|m| !is_multicast(*m)) {
                return Err(Fail::InvalidParameter);
            }
            self.mcast[..list.len()].copy_from_slice(list);
            self.mcast_count = list.len();
        }
        if reset_mcast { self.mcast_count = 0; }
        if setting != self.setting { self.write_filter(b, setting)?; }
        self.setting = setting;
        Ok(())
    }

    /// StationAddress: `reset`, or the NVM's address, is accepted;
    /// any other address is unsupported (see the module comment).
    pub fn station_address<E>(&mut self, reset: bool, new: Option<[u8; 6]>) -> F<(), E> {
        self.initialized()?;
        match (reset, new) {
            (true, _) => {}
            (false, Some(m)) if is_multicast(m) => return Err(Fail::InvalidParameter),
            (false, Some(m)) if m == self.permanent => {}
            (false, Some(_)) => return Err(Fail::Unsupported),
            (false, None) => return Err(Fail::InvalidParameter),
        }
        self.current = self.permanent;
        Ok(())
    }

    /// Whether the setting passes a frame to `destination`.
    pub fn accepts(&self, destination: [u8; 6]) -> bool {
        let s = self.setting;
        if s & PROMISCUOUS != 0 { return true; }
        if destination == BROADCAST_ADDRESS { return s & BROADCAST != 0; }
        if is_multicast(destination) {
            return s & PROMISCUOUS_MULTICAST != 0
                || (s & MULTICAST != 0 && self.mcast[..self.mcast_count].contains(&destination));
        }
        s & UNICAST != 0 && destination == self.current
    }

    /// Length of the next frame the setting passes, dropping the others.
    fn next_accepted<B: Bus>(&mut self, b: &mut B) -> F<Option<usize>, B::Error> {
        while let Some((len, destination)) = self.net.peek(b)? {
            if self.accepts(destination) { return Ok(Some(len)); }
            self.net.skip(b)?;
        }
        Ok(None)
    }

    /// A frame is waiting (WaitForPacket); false unless initialized.
    pub fn frame_waiting<B: Bus>(&mut self, b: &mut B) -> bool {
        self.state == State::Initialized && matches!(self.next_accepted(b), Ok(Some(_)))
    }

    /// Transmit `buf`, whose first `header_size` bytes are filled in here
    /// when it is not 0 (it must then be 14, with `destination` and
    /// `protocol`; `source` defaults to the station address). `token` is
    /// handed back by GetStatus.
    #[allow(clippy::too_many_arguments)]
    pub fn transmit<B: Bus>(&mut self, b: &mut B, header_size: usize, buf: &mut [u8],
        source: Option<[u8; 6]>, destination: Option<[u8; 6]>, protocol: Option<u16>, token: usize)
        -> F<(), B::Error> {
        self.initialized()?;
        if buf.len() > MAX_FRAME { return Err(Fail::InvalidParameter); }
        if header_size != 0 {
            let (Some(d), Some(p)) = (destination, protocol) else { return Err(Fail::InvalidParameter) };
            if header_size != HEADER { return Err(Fail::InvalidParameter); }
            if buf.len() < HEADER { return Err(Fail::BufferTooSmall(HEADER)); }
            buf[0..6].copy_from_slice(&d);
            buf[6..12].copy_from_slice(&source.unwrap_or(self.current));
            buf[12..14].copy_from_slice(&p.to_be_bytes());
        }
        if buf.len() < MIN_FRAME { return Err(Fail::BufferTooSmall(MIN_FRAME)); }
        if self.queued == RECYCLE { return Err(Fail::NotReady); }
        if !self.net.transmit(b, buf)? { return Err(Fail::NotReady); }
        self.recycle[(self.head + self.queued) % RECYCLE] = token;
        self.queued += 1;
        self.transmitted = true;
        Ok(())
    }

    /// Receive the next frame the setting passes into `out`.
    pub fn receive<B: Bus>(&mut self, b: &mut B, out: &mut [u8]) -> F<Received, B::Error> {
        self.initialized()?;
        let Some(len) = self.next_accepted(b)? else { return Err(Fail::NotReady) };
        if len > out.len() { return Err(Fail::BufferTooSmall(len)); }
        match self.net.receive(b, out)? {
            Some(n) => {
                let f = super::ring::frame(&out[..n]);
                Ok(Received { len: n, destination: f.destination, source: f.source, protocol: f.ethertype })
            }
            None => Err(Fail::NotReady),
        }
    }

    /// GetStatus: interrupt bits (RECEIVE when a frame is waiting, TRANSMIT
    /// when one went out since the last call), the link into `media`, and
    /// with `recycle` the oldest transmitted buffer not yet handed back.
    pub fn get_status<B: Bus>(&mut self, b: &mut B, recycle: bool) -> F<(u32, Option<usize>), B::Error> {
        self.initialized()?;
        self.net.reclaim();
        if let Some(link) = self.aq.events(b)? {
            self.link = link;
            self.media = link.up;
        }
        let mut bits = 0;
        if self.next_accepted(b)?.is_some() { bits |= RECEIVE_INTERRUPT; }
        if core::mem::take(&mut self.transmitted) { bits |= TRANSMIT_INTERRUPT; }
        let token = (recycle && self.queued > 0).then(|| {
            let t = self.recycle[self.head];
            self.head = (self.head + 1) % RECYCLE;
            self.queued -= 1;
            t
        });
        Ok((bits, token))
    }

    /// Stop the queues whatever the state (the SNP child going away).
    pub fn halt<B: Bus>(&mut self, b: &mut B) -> Result<(), Error<B::Error>> {
        self.state = State::Stopped;
        self.net.stop(b)
    }

    /// ExitBootServices and binding Stop: nothing of this driver's may DMA
    /// after it. The queues stop, the firmware is told the driver is
    /// unloading, the admin queue is turned off, the HMC page is unhooked,
    /// and the PF is reset. Every step runs even if an earlier one failed.
    /// Ok when the admin queue is off and the PF reset completed (which
    /// stops the queues and clears the HMC descriptor whatever the earlier
    /// steps did); Err: the device may still DMA.
    pub fn teardown<B: Bus>(&mut self, b: &mut B) -> Result<(), Error<B::Error>> {
        self.state = State::Stopped;
        let queues = self.net.stop(b);
        super::release(b, &mut self.aq).or_else(|e| queues.and(Err(e)))
    }
}
