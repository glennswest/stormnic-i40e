//! The PCI functions this driver binds: Intel (8086) physical functions of
//! the 700 series (X710, XL710, XXV710, X722, and their V710/I710 cousins),
//! every one run by the same firmware admin queue.
//!
//! From Intel's shared code (FreeBSD `sys/dev/ixl/i40e_devids.h`, commit
//! 4b732459f030) and checked against pci.ids 2026.11.09, which also names
//! two X722 functions the shared code does not list (37d4, 0dda); they are
//! X722 LOMs with the same programming model and are bound too.
//!
//! Not bound: the virtual functions (154c, 1571, 1889, 37cd, 37d9), which
//! talk to their PF over a mailbox instead of owning an admin queue, and
//! 1574 ("XL710 Emulation", a pre-silicon model that never shipped).

pub const INTEL: u16 = 0x8086;

/// PCI base class of a network controller (config offset 0x0b).
pub const CLASS_NETWORK: u8 = 0x02;

pub struct Nic {
    pub device: u16,
    pub name: &'static str,
}

pub const SUPPORTED: &[Nic] = &[
    // X710 / XL710
    Nic { device: 0x1572, name: "X710 SFP+" },
    Nic { device: 0x1580, name: "XL710 40G backplane" },
    Nic { device: 0x1581, name: "X710 10G backplane" },
    Nic { device: 0x1583, name: "XL710 40G QSFP+" },
    Nic { device: 0x1584, name: "XL710 40G QSFP+" },
    Nic { device: 0x1585, name: "X710 10G QSFP+" },
    Nic { device: 0x1586, name: "X710 10GBASE-T" },
    Nic { device: 0x1587, name: "XL710 20G backplane" },
    Nic { device: 0x1588, name: "XL710 20G backplane" },
    Nic { device: 0x1589, name: "X710/X557-AT 10GBASE-T" },
    Nic { device: 0x15ff, name: "X710 10GBASE-T" },
    Nic { device: 0x104e, name: "X710 10G SFP+" },
    Nic { device: 0x104f, name: "X710 10G backplane" },
    Nic { device: 0x0cf8, name: "X710 N3000" },
    // XXV710
    Nic { device: 0x158a, name: "XXV710 25G backplane" },
    Nic { device: 0x158b, name: "XXV710 25G SFP28" },
    Nic { device: 0x0d58, name: "XXV710 N3000" },
    // V710 / I710 (X710-TL family)
    Nic { device: 0x101f, name: "V710 5GBASE-T" },
    Nic { device: 0x0dd2, name: "I710 1GBASE-T" },
    // X722
    Nic { device: 0x37ce, name: "X722 10G backplane" },
    Nic { device: 0x37cf, name: "X722 10G QSFP+" },
    Nic { device: 0x37d0, name: "X722 10G SFP+" },
    Nic { device: 0x37d1, name: "X722 1G" },
    Nic { device: 0x37d2, name: "X722 10GBASE-T" },
    Nic { device: 0x37d3, name: "X722 10G SFP+" },
    Nic { device: 0x37d4, name: "X722 10G QSFP+" },
    Nic { device: 0x0dda, name: "X722 10G SFP+" },
];

pub fn lookup(vendor: u16, device: u16) -> Option<&'static Nic> {
    if vendor != INTEL { return None; }
    SUPPORTED.iter().find(|n| n.device == device)
}
