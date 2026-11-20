//! Memory decode and bus mastering (src/decode.rs) against a fake PCI
//! function: what `enable` sets, what `release` puts back, and that a failed
//! teardown (DMA region kept) never turns bus mastering back on (#7,
//! finding 704188c2551d).
#[path = "../src/decode.rs"]
#[allow(dead_code)]
mod decode;
use decode::*;
use std::cell::Cell;

type E = &'static str;

/// A PCI function: PciIo attributes mirrored into the command register, as
/// EDK2 does, with switches for the firmware failure modes.
struct Fake {
    attributes: Cell<u64>,
    command: Cell<u16>,
    /// `Attributes(Enable, …)` answers UNSUPPORTED (AMI Aptio 4).
    enable_fails: bool,
    /// `Attributes(Get, …)` answers an error.
    get_fails: bool,
}

impl Fake {
    fn new(attributes: u64, command: u16) -> Self {
        Fake {
            attributes: Cell::new(attributes),
            command: Cell::new(command),
            enable_fails: false,
            get_fails: false,
        }
    }
    fn mirror(&self) {
        let a = self.attributes.get();
        let mut c = self.command.get() & !(COMMAND_MEMORY | COMMAND_BUS_MASTER);
        if a & ATTRIBUTE_MEMORY != 0 {
            c |= COMMAND_MEMORY;
        }
        if a & ATTRIBUTE_BUS_MASTER != 0 {
            c |= COMMAND_BUS_MASTER;
        }
        self.write(c);
    }
    fn write(&self, c: u16) {
        self.command.set(c);
    }
    fn bme(&self) -> bool {
        self.command.get() & COMMAND_BUS_MASTER != 0
    }
    fn mse(&self) -> bool {
        self.command.get() & COMMAND_MEMORY != 0
    }
}

impl Pci for Fake {
    type Status = E;
    fn get(&self) -> Result<u64, E> {
        if self.get_fails { Err("get") } else { Ok(self.attributes.get()) }
    }
    fn supported(&self) -> Result<u64, E> {
        Ok(ATTRIBUTE_MEMORY | ATTRIBUTE_BUS_MASTER | 0x1)
    }
    fn enable(&self, a: u64) -> Result<(), E> {
        if self.enable_fails {
            return Err("unsupported");
        }
        self.attributes.set(self.attributes.get() | a);
        self.mirror();
        Ok(())
    }
    fn disable(&self, a: u64) -> Result<(), E> {
        self.attributes.set(self.attributes.get() & !a);
        self.mirror();
        Ok(())
    }
    fn set(&self, a: u64) -> Result<(), E> {
        self.attributes.set(a);
        self.mirror();
        Ok(())
    }
    fn config_read_u16(&self, offset: u32) -> Result<u16, E> {
        assert_eq!(offset, COMMAND);
        Ok(self.command.get())
    }
    fn config_write_u16(&self, offset: u32, value: u16) -> Result<(), E> {
        assert_eq!(offset, COMMAND);
        self.write(value);
        Ok(())
    }
}

const BOTH: u64 = ATTRIBUTE_MEMORY | ATTRIBUTE_BUS_MASTER;
const BOTH_CMD: u16 = COMMAND_MEMORY | COMMAND_BUS_MASTER;

#[test]
fn enable_sets_memory_and_bus_master() {
    let f = Fake::new(0, 0);
    let e = enable(&f).unwrap();
    assert_eq!(e.by_attributes, BOTH);
    assert!(f.mse() && f.bme());
}

#[test]
fn quiet_release_puts_back_what_was_found() {
    // Off before Start: off again.
    let f = Fake::new(0, 0);
    let e = enable(&f).unwrap();
    release(&f, &e, false).unwrap();
    assert!(!f.mse() && !f.bme());
    // On before Start (a platform driver left it on): on again.
    let f = Fake::new(BOTH | 0x1, BOTH_CMD);
    let e = enable(&f).unwrap();
    release(&f, &e, false).unwrap();
    assert_eq!(f.attributes.get(), BOTH | 0x1);
    assert!(f.mse() && f.bme());
}

/// The finding: bus mastering was on before Start, the device would not tear
/// down, `keep_dma` turned BME off; the restore must not turn it back on.
#[test]
fn kept_dma_release_leaves_bus_mastering_off_when_it_was_on_before() {
    let f = Fake::new(BOTH | 0x1, BOTH_CMD);
    let e = enable(&f).unwrap();
    stop_bus_master(&f).unwrap();
    release(&f, &e, true).unwrap();
    assert!(!f.bme(), "BME back on over a kept DMA region");
    assert!(f.mse(), "memory decode is restored as found");
    assert_eq!(f.attributes.get(), ATTRIBUTE_MEMORY | 0x1, "other attributes restored, BUS_MASTER not");
}

/// Enable refused, so Start set MSE|BME with a config write from a command
/// register that had BME on: the command-register restore must not set it.
#[test]
fn kept_dma_release_by_command_register_leaves_bus_mastering_off() {
    let mut f = Fake::new(0, COMMAND_BUS_MASTER);
    f.enable_fails = true;
    let e = enable(&f).unwrap();
    assert_eq!(e.command_before, Some(COMMAND_BUS_MASTER));
    assert_eq!(e.by_attributes, 0);
    stop_bus_master(&f).unwrap();
    release(&f, &e, true).unwrap();
    assert!(!f.bme());
    assert!(!f.mse());
    // Without a kept region the same function gets its BME back.
    let mut f = Fake::new(0, COMMAND_BUS_MASTER);
    f.enable_fails = true;
    let e = enable(&f).unwrap();
    release(&f, &e, false).unwrap();
    assert!(f.bme() && !f.mse());
}

/// `Get` failed at Start, so release disables what Enable accepted.
#[test]
fn kept_dma_release_without_original_attributes() {
    let mut f = Fake::new(BOTH, BOTH_CMD);
    f.get_fails = true;
    let e = enable(&f).unwrap();
    stop_bus_master(&f).unwrap();
    release(&f, &e, true).unwrap();
    assert!(!f.bme());
}

/// Firmware whose `Set` turns BME back on whatever it is asked: BME is
/// checked and cleared again at the end.
#[test]
fn kept_dma_release_clears_bme_a_set_turned_back_on() {
    let f = Fake::new(BOTH, BOTH_CMD);
    let e = enable(&f).unwrap();
    struct Sticky<'a>(&'a Fake);
    impl Pci for Sticky<'_> {
        type Status = E;
        fn get(&self) -> Result<u64, E> { self.0.get() }
        fn supported(&self) -> Result<u64, E> { self.0.supported() }
        fn enable(&self, a: u64) -> Result<(), E> { self.0.enable(a) }
        fn disable(&self, a: u64) -> Result<(), E> { self.0.disable(a) }
        fn set(&self, a: u64) -> Result<(), E> { self.0.set(a | ATTRIBUTE_BUS_MASTER) }
        fn config_read_u16(&self, o: u32) -> Result<u16, E> { self.0.config_read_u16(o) }
        fn config_write_u16(&self, o: u32, v: u16) -> Result<(), E> { self.0.config_write_u16(o, v) }
    }
    let s = Sticky(&f);
    stop_bus_master(&s).unwrap();
    release(&s, &e, true).unwrap();
    assert!(!f.bme());
}
