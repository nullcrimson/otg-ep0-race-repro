# otg-ep0-race-repro

A reproducer for a race in `embassy-usb-synopsys-otg` on STM32F4, and a check of the proposed
fix. When the USB task runs late, the driver re-arms EP0 OUT at the end of a control transfer
while the next SETUP is half received, and the device's RX FIFO goes out of step.

What that does depends on the version:

- **embassy-usb-synopsys-otg 0.3.3** (the released crate): the device panics in the USB
  interrupt, `panicked at 'assertion failed: ep_num < ep_count'`, or stops answering USB while the
  core keeps running.
- **embassy main**: the RX FIFO stays in step, but EP0 OUT reads go off by one packet; a
  SET_LINE_CODING then gets 0 bytes and is STALLed, and Windows declares the device not
  functioning, within a few control transfers.
- **embassy main plus the fix**: no failure.

```
cargo xtask stream           # embassy main: fails within a few rounds
cargo xtask stream --fixed   # embassy main + the fix: every round passes
```

The firmware is `examples/stm32f4/src/bin/usb_serial.rs` (CDC ACM echo) moved to the
STM32F429I-DISC1's user USB port, plus one interrupt-level task that keeps thread mode busy.

| path | what |
|---|---|
| `firmware/` | the device: CDC ACM echo plus the load, built against embassy main (798395dd); its own Cargo project |
| `firmware-fixed/` | the same `firmware/src/main.rs`, built against embassy main plus the proposed fix; the manifest differs only in the embassy source and rev |
| `xtask/` | the host: builds and flashes a firmware, drives the serial port, reads the core state |

Contents: [How to run](#how-to-run) - [The issue](#the-issue) -
[Proposed fix](#proposed-fix) - [What has been run](#what-has-been-run) - [Details](#details)

## How to run

### Requirements

- An STM32F429I-DISC1 with both cables connected: the ST-LINK mini-USB (flashing, RTT) and the
  micro-AB user USB (the device under test).
- Rust through rustup. `rust-toolchain.toml` pins 1.100 (what embassy main needs) and installs
  the `thumbv7em-none-eabihf` target on first use.
- [probe-rs](https://probe.rs) on `PATH`.
- Tested with a Windows 11 host only. The control transfers each serial call produces, and how a
  failed device behaves, are those of the Windows CDC driver (usbser.sys).

### Commands

From the repository root:

| command | traffic |
|---|---|
| `cargo xtask stream` | port kept open; every round changes the baud rate (115200 / 115207) and DTR, then writes 512 bytes of `ff 01 01 01` |
| `cargo xtask reopen` | every round opens the port, writes `ping`, reads the echo, closes |

Expected:

| firmware | load | result |
|---|---|---|
| embassy main (default) | on (default) | fails within a few rounds |
| embassy main + fix (`--fixed`) | on | every round passes |
| either | off (`--no-load`) | every round passes |

Each command:

1. builds `firmware/` or `firmware-fixed/` (from its own directory, so its `.cargo/config.toml`
   applies) and flashes it with `probe-rs download`;
2. resets the board and waits for the device to enumerate (VID:PID `C0DE:CAFE`);
3. drives the serial port, round after round, on a worker thread;
4. stops at the first round that makes no progress for 3 s, at the first failed serial call (a
   port that fails to open, a wrong echo), or after the last round;
5. reads the core's ICSR register through the probe (`probe-rs read`, which does not halt the
   core) and prints the outcome:
   - `PANIC`: the core is in HardFault, where `panic-probe` ends up after a panic;
   - `HANG`: a round stalled and the core is still running;
   - `FAILED`: a serial call failed and the core is still running.

Options:

| option | effect |
|---|---|
| `--fixed` | build `firmware-fixed/` (embassy main + the fix) instead of `firmware/` (embassy main) |
| `--no-load` | build the firmware without its interrupt-level load: the control run |
| `--rounds N` | stop after N rounds if nothing has failed (N >= 1; defaults: stream 2000, reopen 500) |
| `--drive-only` | only drive the serial port: no build, flash, reset or core read (not with `--fixed` or `--no-load`) |
| `-h`, `--help` | print the usage |

Exit code: 0 every round passed, 1 a round failed (the bug reproduced), 2 the tool itself failed
(build, probe-rs, no device).

The code, in `xtask/src/`:

| file | what |
|---|---|
| `main.rs` | arguments, the steps above, the result line |
| `board.rs` | which firmware, build, flash, reset, read the core state, through the `probe-rs` CLI |
| `port.rs` | find the device by VID:PID and open its serial port |
| `cases.rs` | the traffic of each case, and which word each one got misread on 0.3.3 |
| `watchdog.rs` | run the rounds on a worker thread and notice a stalled one |

### Seeing the firmware's log

`--drive-only` leaves the probe to `probe-rs run`, which prints the firmware's defmt log:

```
# terminal 1: build, flash, run and attach
cd firmware
cargo run --release

# terminal 2, once the log shows "load: busy ..."
cargo xtask stream --drive-only
```

### Example output

```
$ cargo xtask stream
case: stream, embassy main, firmware with load
building firmware
flashing
device on COM20
round 0: could not open COM20: A device attached to the system is not functioning.
result: FAILED (a serial call failed, above; core running, ICSR 00000844)
```

```
$ cargo xtask reopen
case: reopen, embassy main, firmware with load
building firmware
flashing
device on COM20
round 9: could not open COM20: A device attached to the system is not functioning.
result: FAILED (a serial call failed, above; core running, ICSR 00000844)
```

```
$ cargo xtask stream --fixed
case: stream, embassy main + the fix, firmware with load
building firmware
flashing
device on COM20
500 rounds done
1000 rounds done
1500 rounds done
2000 rounds done
result: 2000 rounds, no failure
```

```
$ cargo xtask reopen --fixed
case: reopen, embassy main + the fix, firmware with load
building firmware
flashing
device on COM20
500 rounds done
result: 500 rounds, no failure
```

ICSR `0x844` is active exception 68, UART4: the load task, preempting thread mode as it always
does. Cargo's and probe-rs's own progress lines are left out above.

## The issue

### Mechanism

All OUT endpoints share one RX FIFO. For every packet the core pushes a status word (endpoint,
byte count, packet type) and then the data; the interrupt handler pops the status word from
GRXSTSP and reads the data that belongs to it. A SETUP produces two entries: SETUP_DATA_RX
(the 8 bytes) and then SETUP_DONE, after which the core raises STUP. RM0090 hands EP0 back to
software at STUP.

`Endpoint<Out>::read` re-arms the endpoint after every packet it returns, EP0 included: it writes
DOEPTSIZ and sets CNAK on DOEPCTL0. For a control-IN transfer (GET_LINE_CODING, a descriptor
read) the last packet on EP0 OUT is the zero-length status packet, and the re-arm after it serves
no transfer: the next thing on EP0 is a SETUP, which the core accepts whatever the NAK state.

That re-arm runs in thread mode, in the USB device task, after the interrupt has popped the
status packet and woken the task. When thread mode is late, the host has already started the next
control transfer: the core has received its SETUP and set NAK on EP0 OUT, and the interrupt has
popped SETUP_DATA_RX, but SETUP_DONE and STUP have not come yet. The late CNAK clears that NAK
before STUP. From then on the RX FIFO is out of step: the handler pops a data word where a status
word is due.

The trigger needs nothing but back-to-back control transfers and a thread-mode USB task that runs
late by the right amount: an open of a CDC ACM port sends three control transfers in a row.

Embassy main still re-arms EP0 OUT after the status stage (`read` = `read_packet` + `arm_out`).

### Trace (0.3.3)

The GRXSTSP sequence, captured with a ring buffer in an instrumented build of
embassy-usb-synopsys-otg 0.3.3 on the same chip (times in CPU cycles at 168 MHz):

| step | who | what | GRXSTSP |
|---|---|---|---|
| 1 | ISR | pops the previous transfer's status ZLP on EP0, wakes the task | `0x01250000`, `0x01270000` |
| 2 | ISR | pops the next SETUP: SETUP_DATA_RX, EP 0, BCNT 8 | `0x012c0080` |
| 3 | thread | `Endpoint::read` returns the step-1 ZLP, re-arms EP0 OUT: CNAK (~3,700 cycles after step 2) | |
| 4 | ISR | next pop, due to be SETUP_DONE (`0x01280000` in every good transfer, 8-10k cycles after step 2) | `0x010101ff` |
| 5 | ISR | decodes EPNUM 15, BCNT 31, PKTSTS 0 (reserved); `assert!(ep_num < ep_count)` fails | |

The same sequence (status ZLP, SETUP_DATA_RX, late CNAK, then a word out of place) preceded every
failure recorded on 0.3.3 with this repository's two traffic patterns.

### Outcomes on 0.3.3

What happens next depends only on the word misread as a status word, and so on what is in the
FIFO at that moment:

| outcome | traffic that gave it | misread word (recorded) | decodes as | result |
|---|---|---|---|---|
| panic | `stream` | bulk data `ff 01 01 01` = `0x010101ff`, or SET_LINE_CODING data, whose first word is the baud rate: 115207 = `0x0001c207` | endpoint 15 / endpoint 7: past the endpoint count | the assert fires; `panic-probe` halts the core |
| hang | `reopen` | GET_LINE_CODING request `a1 21 00 00` = `0x000021a1` | endpoint 1 (exists), reserved packet status | the assert passes, the status is ignored, the driver carries on out of step; EP0 never completes another transfer |
| wrong data (not reproduced) | | a word that decodes as OUT_DATA_RX for an existing endpoint | a valid packet | the driver copies the following FIFO words into that endpoint as a packet the host never sent |

At a hang, read with the core running: DOEPCTL0 = `0x00008000` (EP0 OUT active, not enabled),
DOEPTSIZ0 = `0x60000022` (STUPCNT 3, PKTCNT 0), DIEPCTL0 = `0x00008000`, RX FIFO empty, no
endpoint interrupt pending: EP0 is idle while the host's control transfer is still pending.

With probe-rs attached, the 0.3.3 panic:

```
0.000007 [INFO ] load: busy 100..180 us every 200 us
4.595221 [ERROR] panicked at 'assertion failed: ep_num < ep_count' (embassy_usb_synopsys_otg embassy-usb-synopsys-otg-0.3.3/src/lib.rs:53)
Firmware exited unexpectedly: Multiple
    Frame 0: HardFault_ @ 0x08007ea6
```

### On embassy main

Main aborts a control stage when a new SETUP arrives (`abort_on_new_setup_or_reset`, from
#7442). The same ring trace on main (8 failures, 4 per case; the read and reject hooks were in the last
6) shows the late task triggering that
abort, the FIFO staying in step, and the failure coming from what the abort leaves behind:

1. A control-IN transfer's status ZLP is popped by the interrupt into the EP0 OUT buffer.
2. The USB task is late; the next SETUP arrives first, and the waiting status stage is aborted.
   The ZLP stays in the buffer.
3. From then on each EP0 OUT read returns the previous transfer's packet. A status read returns a
   few hundred cycles after the IN data is queued, before that data has even gone out.
4. A SET_LINE_CODING's data stage reads the leftover 0-byte packet instead of its 7 bytes, and the
   request is rejected (STALL). Windows then declares the device not functioning.

```
694 setup taken      00002021   SET_LINE_CODING, 7-byte data stage
696 read_packet      00000700   EP0, buffer 7 bytes, got 0: the leftover status ZLP
698 REJECT (STALL)
    a good one:      00000707   got the 7 bytes, request accepted
```

In all 8: no status word out of step, and 2-4 aborted stages. In the 6 with read hooks: the
off-by-one read before each failing SET_LINE_CODING. The late CNAK inside a SETUP window, the 0.3.3 trigger, was seen once
on main, with the FIFO staying in step that time. The fix empties the EP0 OUT buffer when a SETUP
is taken and no longer re-arms after the status stage, which removes both.

### Upstream

- [#3493](https://github.com/embassy-rs/embassy/issues/3493): the same assert, on reopening a
  serial port; gone with defmt trace logging on, which changes the timing.
- [#3565](https://github.com/embassy-rs/embassy/pull/3565): moved the CNAK that starts the data
  stage to STUP; the status-stage re-arm stayed.

## Proposed fix

One change to `embassy-usb-synopsys-otg`, one commit on embassy main:
[synopsys-otg: do not re-arm EP0 OUT after the status stage of a control transfer](https://github.com/nullcrimson/embassy/pull/1)
(commit `765d3c33`, parent `798395dd`).

- The status stage reads its packet without re-arming the endpoint.
- The SETUP handler arms EP0 OUT for the new transfer, in one critical section with the read of
  the SETUP bytes, and not while the RX FIFO still holds an entry the interrupt has not read (it
  can be a newer SETUP).
- A flag set when SETUP_DATA_RX is popped lets the control pipe abort a stage from that moment
  instead of from STUP.
- The SETUP handler empties the EP0 OUT buffer, so a status packet left by an aborted stage is not
  read by the next transfer as its own.

`firmware-fixed/` builds this repository's firmware against that commit: embassy main plus this
one commit, nothing else. `cargo xtask <case> --fixed` runs a case against it.

## What has been run

STM32F429I-DISC1, Windows 11, board reset before each trial. "Load" is the load in `firmware/`
(100-180 us busy every 200 us). A trial ends at the first failed round or after its last round.

### embassy main, as published here

| # | embassy | load | case | trials | result |
|---|---|---|---|---|---|
| 1 | main 798395dd | on | `stream` | 4 | FAILED at round 0, 3, 0, 0 |
| 2 | main 798395dd | on | `reopen` | 4 | FAILED at round 1, 2, 1, 9 |
| 3 | main + fix 765d3c33 | on | `stream` | 4 | 2,000 rounds each, no failure |
| 4 | main + fix 765d3c33 | on | `reopen` | 4 | 500 rounds each, no failure |
| 5 | main 798395dd | off | `stream` | 1 | 2,000 rounds, no failure |
| 6 | main 798395dd | off | `reopen` | 1 | 500 rounds, no failure |

Before the xtask had `--fixed`, the same two firmwares were built from local checkouts of the same
two commits and driven with `--drive-only`:

| # | embassy | load | case | trials | result |
|---|---|---|---|---|---|
| 7 | main 798395dd | on | `stream` | 4 | FAILED at round 0, 0, 0, 1 |
| 8 | main 798395dd | on | `reopen` | 4 | FAILED at round 3, 2, 1, 1 |
| 9 | main + fix 765d3c33 | on | `stream` | 2 | 1,000 rounds each, no failure |
| 10 | main + fix 765d3c33 | on | `reopen` | 2 | 1,000 rounds each, no failure |
| 11 | main 798395dd | off | both | 1 each | 300 rounds each, no failure |
| 12 | main 798395dd + ring trace | on | `stream` | 4 | FAILED at round 0, 1, 1, 0; no status word out of step; in the 3 with read and reject hooks, an off-by-one EP0 read and a SET_LINE_CODING STALL before the failure |
| 13 | main 798395dd + ring trace | on | `reopen` | 4 | FAILED at round 1, 2, 3, 1; same as 12 |

### embassy-usb-synopsys-otg 0.3.3, earlier

The firmware was then built against the crates.io releases (embassy-stm32 0.6.0,
embassy-usb-synopsys-otg 0.3.3); its source differed from today's `firmware/src/main.rs` only where
embassy main's API changed: the rcc enum names, the 48 MHz clock mux, the order of `new_fs`'s
arguments and the `cdc_acm` module path. Runs from a Python host script are the same traffic as
the xtask case named. "Patched 0.3.3" is the same fix applied to 0.3.3, outside this repository.

| # | driver | load | case | host | trials | result |
|---|---|---|---|---|---|---|
| 14 | 0.3.3 | on | `stream` | xtask | 4 | PANIC at round 3, 21, 12, 12 |
| 15 | 0.3.3 | off | `stream` | xtask | 1 | 2,000 rounds, no failure |
| 16 | 0.3.3 | on | `stream` | xtask `--drive-only`, `probe-rs run` attached | 1 | panic message printed (above), round 2 |
| 17 | 0.3.3 | on | `reopen` | xtask | 4 | HANG at round 14, 9, 7, 8 |
| 18 | 0.3.3 | off | `reopen` | xtask | 1 | 500 rounds, no failure |
| 19 | 0.3.3 | on | `stream` | Python | 3 | PANIC at round 3, 2, 8 |
| 20 | 0.3.3 | off | `stream` | Python | 1 | 20,000 rounds, no failure |
| 21 | patched 0.3.3 | on | `stream` | Python | 3 | 20,000 rounds each, no failure |
| 22 | 0.3.3 | on | `reopen` | Python | 3 | HANG at round 15, 8, 2 |
| 23 | 0.3.3 | off | `reopen` | Python | 1 | 3,000 rounds, no failure |
| 24 | patched 0.3.3 | on | `reopen` | Python | 3 | 3,000 rounds each, no failure |
| 25 | 0.3.3 | 800 us busy of every 1 ms | `reopen` | Python | 2 | 2,000 and 20,000 rounds, no failure |
| 26 | 0.3.3 | 0-3 ms busy, 0-0.5 ms gaps, random | `reopen` | Python | 1 | ~5,000 rounds, no failure |
| 27 | 0.3.3 + ring trace | on | `reopen`, and variants (baud change, `ff 01 01 01` echo) | Python | 3 | HANG each time; the trace shows the sequence above, misread word `0x000021a1` |
| 28 | 0.3.3 + ring trace | on | `stream` | Python | 3 | PANIC each time; misread word `0x010101ff` once, `0x0001c207` twice |
| 29 | 0.3.3 | on | wrong-data attempt: baud 262209 = `0x00040041` (decodes as EP1 OUT_DATA_RX, 4 bytes), 56-byte echo rounds | Python | 10 | HANG at rounds 15-559; no bytes the host did not send were echoed |
| 30 | 0.3.3 + ring trace | on | as 29 | Python | 1 | HANG, by a different path: one SETUP handled twice, FIFO never out of step |

Rows 25 and 26: long bursts never failed. The thread then resumes long after the next SETUP has
been fully handled, where a late re-arm is harmless; the window needs the thread to resume between
SETUP_DATA_RX and SETUP_DONE.

### Not run yet

- Hosts other than Windows 11.
- Chips other than the STM32F429.

## Details

### The load

`firmware/src/main.rs`, top:

```rust
const PERIOD_US: u64 = 200;
const BUSY_MIN_US: u32 = 100;
const BUSY_SPREAD_US: u32 = 80;
```

An `InterruptExecutor` on UART4 (unused as a peripheral here), at priority 6, runs a task that
spins for 100-180 us (pseudo-random) once every 200 us. OTG_HS stays at priority 0 and preempts
it, so the USB interrupt is on time and only thread mode, where `usb.run()` lives, is late, by up
to ~180 us at a time, many times per control transfer. The `no-load` feature skips starting the
executor.

### Hardware

STM32F429I-DISC1 (STM32F429ZI, 8 MHz HSE, on-board ST-LINK/V2-B). The user USB is USB_OTG_HS in
full-speed mode on its embedded PHY (PB14 DM, PB15 DP), no VBUS sensing. Clocks: 168 MHz SYSCLK,
48 MHz USB from PLLQ. The device enumerates as `USB VID:PID=C0DE:CAFE`, product "OTG EP0 race
repro", serial `12345678`.

### Versions

| | `firmware/` | `firmware-fixed/` |
|---|---|---|
| embassy | `embassy-rs/embassy` at `798395ddab842137845179a7d78cf28913b28d42` (main) | `nullcrimson/embassy` at `765d3c3360f1b478e8aceaf095020049157230e5` (that commit plus the fix) |
| embassy-usb-synopsys-otg | 0.4.0 | 0.4.0 + the fix |
| embassy-stm32 / embassy-usb / embassy-executor / embassy-time | 0.6.0 / 0.6.0 / 0.10.0 / 0.5.1 | same |
| rustc | 1.100 (`rust-toolchain.toml`) | same |

### Debug logging hides it

`DEFMT_LOG=trace` makes the driver log every GRXSTSP word, but logging that much from the
interrupt changes the timing enough to hide the bug (as in #3493). The traces above come from a
ring buffer of 32-bit words, read with `probe-rs read` after the failure.

## License

Derived from an embassy example. Licensed under either of [Apache License, Version
2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
