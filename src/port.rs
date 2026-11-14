//! The port's media, its standard FEC and the one line it reports: the
//! stormnic contract (stormnic-i40e#5, stormbootx#202). The driver brings
//! its port up itself, with the IEEE 802.3 FEC for the media and speed;
//! nothing about FEC, speed or the port is handed down by the loader.
//!
//! The media is the PHY type Get Link Status reports; the FEC is configured
//! with Get PHY Abilities (the current configuration) and Set PHY Config
//! (the same configuration back, FEC changed, link restarted atomically);
//! the FEC that runs is Get Link Status's config byte (docs/hardware.md).

use super::adminq::{self, AdminQ, PhyConfig};
use super::{Bus, Error, Link};

impl Link {
    /// The FEC the link runs (Get Link Status config byte).
    pub fn fec(&self) -> Fec {
        if self.config & adminq::LINK_FEC_RS != 0 {
            Fec::Rs
        } else if self.config & adminq::LINK_FEC_KR != 0 {
            Fec::BaseR
        } else {
            Fec::None
        }
    }

    /// A module or cable is there (the firmware says so, or the link is up).
    pub fn media(&self) -> bool { self.up || self.info & adminq::MEDIA_AVAILABLE != 0 }
}

/// Forward error correction on the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fec {
    None,
    /// BASE-R (FireCode), IEEE 802.3 Clause 74.
    BaseR,
    /// RS(528,514), IEEE 802.3 Clause 108.
    Rs,
    /// Configured to let the firmware choose (never what a link runs).
    Auto,
}

impl Fec {
    pub fn name(self) -> &'static str {
        match self {
            Fec::None => "none",
            Fec::BaseR => "base-r",
            Fec::Rs => "rs",
            Fec::Auto => "auto",
        }
    }

    /// The FEC configuration byte that asks for exactly this FEC.
    pub fn config(self) -> u8 {
        use adminq::*;
        match self {
            Fec::None => 0,
            Fec::BaseR => FEC_ABILITY_KR | FEC_REQUEST_KR,
            Fec::Rs => FEC_ABILITY_RS | FEC_REQUEST_RS,
            Fec::Auto => FEC_AUTO | FEC_ABILITY_KR | FEC_ABILITY_RS,
        }
    }

    /// What an FEC configuration byte asks for.
    pub fn of_config(c: u8) -> Fec {
        use adminq::*;
        let kr = c & (FEC_ABILITY_KR | FEC_REQUEST_KR) != 0;
        let rs = c & (FEC_ABILITY_RS | FEC_REQUEST_RS) != 0;
        match (c & FEC_AUTO != 0, kr, rs) {
            (false, false, false) => Fec::None,
            (false, true, false) => Fec::BaseR,
            (false, false, true) => Fec::Rs,
            _ => Fec::Auto,
        }
    }
}

/// Get Link Status PHY types with no runnable media behind them.
pub const PHY_UNRECOGNIZED: u8 = 0x0e;
pub const PHY_UNSUPPORTED: u8 = 0x0f;
pub const PHY_EMPTY: u8 = 0xfe;

/// The media of a Get Link Status PHY type and the speed it runs (Mb/s).
pub fn media(phy_type: u8) -> Option<(&'static str, u32)> {
    Some(match phy_type {
        0x00 => ("SGMII", 1_000),
        0x01 => ("1000BASE-KX", 1_000),
        0x02 => ("10GBASE-KX4", 10_000),
        0x03 => ("10GBASE-KR", 10_000),
        0x04 => ("40GBASE-KR4", 40_000),
        0x05 => ("XAUI", 10_000),
        0x06 => ("XFI", 10_000),
        0x07 => ("SFI", 10_000),
        0x08 => ("XLAUI", 40_000),
        0x09 => ("XLPPI", 40_000),
        0x0a => ("40GBASE-CR4 copper", 40_000),
        0x0b => ("10GBASE-CR1 copper", 10_000),
        0x0c => ("10GBASE AOC", 10_000),
        0x0d => ("40GBASE AOC", 40_000),
        0x11 => ("100BASE-TX", 100),
        0x12 => ("1000BASE-T", 1_000),
        0x13 => ("10GBASE-T", 10_000),
        0x14 => ("10GBASE-SR", 10_000),
        0x15 => ("10GBASE-LR", 10_000),
        0x16 => ("10GBASE SFP+ direct attach", 10_000),
        0x17 => ("10GBASE-CR1", 10_000),
        0x18 => ("40GBASE-CR4", 40_000),
        0x19 => ("40GBASE-SR4", 40_000),
        0x1a => ("40GBASE-LR4", 40_000),
        0x1b => ("1000BASE-SX", 1_000),
        0x1c => ("1000BASE-LX", 1_000),
        0x1d => ("1000BASE-T SFP", 1_000),
        0x1e => ("20GBASE-KR2", 20_000),
        0x1f => ("25GBASE-KR", 25_000),
        0x20 => ("25GBASE-CR", 25_000),
        0x21 => ("25GBASE-SR", 25_000),
        0x22 => ("25GBASE-LR", 25_000),
        0x23 => ("25GBASE AOC", 25_000),
        0x24 => ("25GBASE ACC", 25_000),
        0x26 | 0x30 => ("2.5GBASE-T", 2_500),
        0x27 | 0x31 => ("5GBASE-T", 5_000),
        _ => return None,
    })
}

/// The IEEE 802.3 FEC for a port's media and speed. 25GBASE-SR/CR/LR/KR
/// (and the 25G AOC/ACC modules, whose host side runs the same way) take
/// RS-FEC, Clause 108. The 1G, 2.5G, 5G, 10G, 20G and 40G media of this
/// family run without FEC: "none" is their standard. An unknown PHY type
/// gets none: nothing is forced on it.
pub fn standard_fec(phy_type: u8) -> Fec {
    match media(phy_type) {
        Some((_, 25_000)) => Fec::Rs,
        _ => Fec::None,
    }
}

/// What the bring-up did about the port's FEC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FecSet<E> {
    /// The port already runs (link up) or is configured for (link down) the
    /// standard FEC: nothing written.
    Kept,
    /// Set PHY Config wrote the standard FEC; the link restarted on it.
    Written,
    /// No module or cable, or one the firmware does not run: nothing to set
    /// the FEC for.
    NoMedia,
    /// The firmware's PHY configuration lists no PHY type; writing it back
    /// would turn the link off, so nothing was written.
    NoPhyTypes,
    /// Get PHY Abilities failed.
    Unread(Error<E>),
    /// Set PHY Config was refused.
    Refused(Error<E>),
}

/// The port's FEC as the bring-up found and left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortFec<E> {
    /// The media's PHY type, from Get Link Status.
    pub phy_type: u8,
    pub standard: Fec,
    /// The PHY configuration before (None: not read).
    pub before: Option<PhyConfig>,
    pub set: FecSet<E>,
}

impl<E> PortFec<E> {
    /// The FEC the port is configured for now (None: not known).
    pub fn configured(&self) -> Option<Fec> {
        match self.set {
            FecSet::Written => Some(self.standard),
            _ => self.before.map(|c| Fec::of_config(c.fec)),
        }
    }

    /// The firmware error, if the FEC could not be read or set.
    pub fn error(&self) -> Option<&Error<E>> {
        match &self.set {
            FecSet::Unread(e) | FecSet::Refused(e) => Some(e),
            _ => None,
        }
    }
}

/// Bring the port's FEC to the standard for its media: read the PHY
/// configuration and, unless the port already runs (link up) or is
/// configured for (link down) the standard FEC, write it back with the
/// standard FEC. Never fails the bring-up: a firmware that will not is
/// reported on the port's line.
pub fn port_fec<B: Bus>(b: &mut B, aq: &mut AdminQ, link: &Link) -> PortFec<B::Error> {
    let phy_type = link.phy_type;
    let standard = standard_fec(phy_type);
    let mut fec = PortFec { phy_type, standard, before: None, set: FecSet::Kept };
    if !link.media() || media(phy_type).is_none() {
        fec.set = FecSet::NoMedia;
        return fec;
    }
    let mut c = match aq.phy_config(b) {
        Ok(c) => c,
        Err(e) => {
            fec.set = FecSet::Unread(e);
            return fec;
        }
    };
    fec.before = Some(c);
    let standard_now = if link.up { link.fec() == standard } else { Fec::of_config(c.fec) == standard };
    if standard_now {
        return fec;
    }
    if c.phy_types == 0 && c.phy_types_ext == 0 {
        fec.set = FecSet::NoPhyTypes;
        return fec;
    }
    c.fec = standard.config();
    fec.set = match aq.set_phy_config(b, &c) {
        Ok(()) => FecSet::Written,
        Err(e) => FecSet::Refused(e),
    };
    fec
}

/// The one line a port reports at bind: driver, PCI address, PCI ID, part,
/// port, MAC, media, speed, FEC and link. A port without link, or off its
/// standard FEC, is an `error:` line, never a quiet one. `detail` says the
/// firmware error when the FEC could not be read or set.
pub struct PortLine<'a, A: core::fmt::Display, E> {
    pub version: &'a str,
    pub at: A,
    pub device: u16,
    pub part: &'a str,
    pub port: u32,
    pub mac: [u8; 6],
    pub link: Link,
    pub fec: &'a PortFec<E>,
    pub waited_ms: usize,
    pub detail: Option<&'a dyn core::fmt::Display>,
}

impl<A: core::fmt::Display, E> PortLine<'_, A, E> {
    /// Why the FEC was not set, if it was not.
    fn why(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let what = match self.fec.set {
            FecSet::NoPhyTypes => return f.write_str("; FEC not set: the firmware lists no PHY type"),
            FecSet::Unread(_) => "; FEC not read",
            FecSet::Refused(_) => "; FEC not set (Set PHY Config)",
            _ => return Ok(()),
        };
        f.write_str(what)?;
        match self.detail {
            Some(d) => write!(f, ": {d}"),
            None => Ok(()),
        }
    }
}

impl<A: core::fmt::Display, E> core::fmt::Display for PortLine<'_, A, E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (m, l) = (self.mac, self.link);
        write!(
            f,
            "stormnic-i40e {}: {} 8086:{:04x} {} port {}: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, media ",
            self.version, self.at, self.device, self.part, self.port, m[0], m[1], m[2], m[3], m[4], m[5]
        )?;
        let name = media(l.phy_type).map(|(n, _)| n);
        match (l.media(), name) {
            (false, _) => f.write_str("none")?,
            (true, Some(n)) => f.write_str(n)?,
            (true, None) => write!(f, "PHY type {:#04x}", l.phy_type)?,
        }
        let standard = self.fec.standard;
        if l.up {
            match l.mbps {
                0 => f.write_str(", speed unknown")?,
                s if s.is_multiple_of(1000) => write!(f, ", speed {} Gb/s", s / 1000)?,
                s => write!(f, ", speed {s} Mb/s")?,
            }
            let running = l.fec();
            write!(f, ", FEC {}, link UP", running.name())?;
            if running != standard {
                write!(f, ": error: the standard FEC for {} is {}", name.unwrap_or("this media"), standard.name())?;
                if matches!(self.fec.set, FecSet::Written) {
                    f.write_str("; it was set, the firmware runs another")?;
                }
                self.why(f)?;
            }
            return Ok(());
        }
        let configured = self.fec.configured().map(Fec::name).unwrap_or("unknown");
        write!(f, ", speed none, FEC {configured}, link DOWN: error: ")?;
        if !l.media() {
            return f.write_str("no module or cable (the firmware reports no media)");
        }
        if name.is_none() {
            let kind = match l.phy_type {
                PHY_UNSUPPORTED => ", unsupported module",
                PHY_UNRECOGNIZED => ", unrecognized module",
                _ => "",
            };
            return write!(f, "the firmware does not run this media (PHY type {:#04x}{kind})", l.phy_type);
        }
        write!(f, "no link after {} ms", self.waited_ms)?;
        self.why(f)?;
        if standard == Fec::Rs {
            f.write_str(" (the link partner must run RS-FEC too, IEEE 802.3 Clause 108; check the cable and the switch port)")
        } else {
            f.write_str(" (check the cable and the link partner)")
        }
    }
}
