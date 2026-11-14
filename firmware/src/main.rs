#![no_std]
#![no_main]

use defmt::{panic, *};
use defmt_rtt as _;
use embassy_executor::{InterruptExecutor, Spawner};
use embassy_futures::join::join;
use embassy_stm32::interrupt::{InterruptExt, Priority};
use embassy_stm32::time::Hertz;
use embassy_stm32::usb::{Driver, Instance};
use embassy_stm32::{Config, bind_interrupts, interrupt, peripherals, usb};
use embassy_time::{Duration, Ticker, block_for};
use embassy_usb::Builder;
use embassy_usb::class::cdc_acm::device::{CdcAcmClass, Config as CdcAcmConfig, State};
use embassy_usb::driver::EndpointError;
use panic_probe as _;

// Every PERIOD_US, an interrupt-level task spins for BUSY_MIN_US plus up to BUSY_SPREAD_US, so
// thread mode, where the USB device task runs, is late by up to ~180 us many times a transfer.
const PERIOD_US: u64 = 200;
const BUSY_MIN_US: u32 = 100;
const BUSY_SPREAD_US: u32 = 80;

bind_interrupts!(struct Irqs {
    OTG_HS => usb::InterruptHandler<peripherals::USB_OTG_HS>;
});

static LOAD_EXECUTOR: InterruptExecutor = InterruptExecutor::new();

#[interrupt]
unsafe fn UART4() {
    // SAFETY: UART4 is not used as a peripheral here; it is the one interrupt LOAD_EXECUTOR is
    // started on, so this is the only context that polls it.
    unsafe { LOAD_EXECUTOR.on_interrupt() }
}

#[embassy_executor::task]
async fn load() {
    let mut ticker = Ticker::every(Duration::from_micros(PERIOD_US));
    let mut seed: u32 = 0x1234_5678;
    loop {
        ticker.next().await;
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        block_for(Duration::from_micros(u64::from(BUSY_MIN_US + seed % BUSY_SPREAD_US)));
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    info!("Hello World!");

    let mut config = Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Oscillator,
        });
        config.rcc.pll_src = PllSource::Hse;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::Div4,
            mul: PllMul::Mul168,
            divp: Some(PllPDiv::Div2), // 8mhz / 4 * 168 / 2 = 168Mhz.
            divq: Some(PllQDiv::Div7), // 8mhz / 4 * 168 / 7 = 48Mhz.
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::Div1;
        config.rcc.apb1_pre = APBPrescaler::Div4;
        config.rcc.apb2_pre = APBPrescaler::Div2;
        config.rcc.sys = Sysclk::Pll1P;
        config.rcc.mux.clk48sel = mux::Clk48sel::Pll1Q;
    }
    let p = embassy_stm32::init(config);

    // OTG_HS keeps its reset priority (0, highest): the USB interrupt preempts the load; only
    // thread mode is starved.
    if cfg!(not(feature = "no-load")) {
        interrupt::UART4.set_priority(Priority::P6);
        let load_spawner = LOAD_EXECUTOR.start(interrupt::UART4);
        load_spawner.spawn(unwrap!(load()));
        info!("load: busy {}..{} us every {} us", BUSY_MIN_US, BUSY_MIN_US + BUSY_SPREAD_US, PERIOD_US);
    } else {
        info!("load: off");
    }

    // USB_OTG_HS with its embedded full-speed PHY on PB15 (DP) / PB14 (DM).
    let mut ep_out_buffer = [0u8; 256];
    let mut config = embassy_stm32::usb::Config::default();
    config.vbus_detection = false;
    let driver = Driver::new_fs(
        p.USB_OTG_HS,
        p.PB15,
        p.PB14,
        Irqs,
        &mut ep_out_buffer,
        config,
    );

    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("OTG EP0 race repro");
    config.serial_number = Some("12345678");

    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut control_buf = [0; 64];

    let mut state = State::new();

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut [], // no msos descriptors
        &mut control_buf,
    );

    let mut class = CdcAcmClass::new(&mut builder, &mut state, CdcAcmConfig::new());

    let mut usb = builder.build();

    let usb_fut = usb.run();

    let echo_fut = async {
        loop {
            class.wait_connection().await;
            info!("Connected");
            let _ = echo(&mut class).await;
            info!("Disconnected");
        }
    };

    join(usb_fut, echo_fut).await;
}

struct Disconnected {}

impl From<EndpointError> for Disconnected {
    fn from(val: EndpointError) -> Self {
        match val {
            EndpointError::BufferOverflow => panic!("Buffer overflow"),
            EndpointError::Disabled => Disconnected {},
        }
    }
}

async fn echo<'d, T: Instance + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, T>>,
) -> Result<(), Disconnected> {
    let mut buf = [0; 64];
    loop {
        let n = class.read_packet(&mut buf).await?;
        class.write_packet(&buf[..n]).await?;
    }
}
