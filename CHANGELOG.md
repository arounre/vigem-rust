# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1]

### Added

- `BusError::UserIndexUnavailable`: returned by `user_index()` when the host has assigned an XInput
  slot but the driver did not return its number (see Fixed).

### Deprecated

- `TargetHandle::user_index()`. Current ViGEmBus drivers do not return the number. Use
  `register_notification` and `X360Notification::led_number`, which carries the same slot number
  on every driver.

### Fixed

- **DS4 `wait_for_ready` failed on a slow or busy system.** ViGEmBus waits at most 1 s for the
  host to start a new pad. When the host needed longer, `plug` returned without the driver's
  confirmation and the pad was still being started: for a moment the driver answered a report
  with "no such device", then "busy", before the usual `TargetNotReady`. `wait_for_ready` retried
  only `TargetNotReady`, so it returned `BusError::DeviceGone` or `BusError::Ioctl` (Win32 170)
  for a pad that accepted reports a few hundred milliseconds later. It now retries all three
  until a report is accepted or the timeout passes. A pad the driver still does not know at the
  timeout returns `DeviceGone`, as in the Xbox 360 wait. An `update` sent straight after `plug`
  without `wait_for_ready` can still see these errors.
- **DS4 raw output reports went to the wrong pad.** `register_notification_raw_buffer` ran one
  `IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE` listener per pad and assumed every completion was its own.
  The driver ignores the serial in that request and hands each output report, from any DS4 pad on
  the bus, to one pending request per bus handle, stamped with the originating serial. With two
  subscribed DS4 pads on one `Client`, a report for one could arrive on the other's receiver and
  never on its own; with another process running a DS4 pad, that pad's reports showed up on yours.
  Now one listener per `Client` routes each report by its serial, matching upstream ViGEmClient.
  Reports for other pads and other processes are discarded. No API change. The typed
  `register_notification` stream was not affected.
- **A new DS4 pad could receive a previous pad's raw output reports.** ViGEmBus keeps the newest
  64 DS4 output reports per open bus handle and does not clear them when a pad is unplugged. They
  carry only the pad's serial, so the first `register_notification_raw_buffer` subscriber on a new
  pad got whatever had been sent to an earlier pad with the same serial (yours or another
  process's), for example an old "rumble on". The raw-output listener now uses its own bus handle,
  which is replaced by a newly opened (empty) one just before each DS4 plug. Output the host sent
  to the pad itself before you subscribed is still delivered, except when another DS4 pad of the
  same `Client` was subscribed or plugged in the meantime.
- **A new notification subscription could be disconnected at once.** If the last
  `NotificationReceiver` of a pad was dropped, a notification arrived, and another thread called
  `register_notification` at that moment, the new receiver could be attached to the listener that
  was just stopping and report `Disconnected` on a pad that was still plugged in. Such a
  registration now starts a new listener.
- **`update` could send a report after the pad was unplugged.** A call that waited for another
  clone's `update` on the same pad checked that the pad was plugged in only before waiting. If the
  pad was unplugged meanwhile, the report was still sent to its serial, which another pad may have
  taken over. The check is now repeated after the wait.
- **Xbox 360 `wait_for_ready` did not check anything.** It sent all-zero reports and waited for
  three to succeed, but the driver accepts an unchanged report without touching the device, and a
  new pad's last report is all zeroes. So the check always passed. On drivers that implement
  `WAIT_DEVICE_READY` this did no harm, because `plug` had already waited for the host. On drivers
  that don't, you got a `Ready` handle with no evidence the host had seen the pad. Now `plug`
  records whether the driver confirmed readiness. If it did, `wait_for_ready` returns at once. If
  not, it polls the pad's XInput user index, which the driver only reports after the host's first
  LED packet. It no longer sends any reports. When it has to poll, a fifth or later Xbox 360 pad
  may never get an index; use `assume_ready` for those. If the timeout passes it returns
  `ReadyTimeout`, or `BusError::DeviceGone` if the driver still says the pad does not exist.
- **`user_index()` failed with a made-up `ERROR_INSUFFICIENT_BUFFER`.** Current ViGEmBus drivers
  (measured on 1.21.442) answer the user-index query successfully but return no data, and this
  crate turned that into an "insufficient buffer" error. The number cannot be read from that
  driver call. `user_index()` now returns the new `BusError::UserIndexUnavailable` in that case,
  and its docs say what it can and cannot do. Before the host assigns a slot it still fails with
  Win32 650.
- **Registering for notifications could panic.** If Windows refused to start the listener thread,
  `register_notification` and `register_notification_raw_buffer` panicked inside the library. They
  now return `BusError::Ioctl` (operation `spawn notification worker`). Listener threads are named
  `vigem-notify-{serial}` (per pad) and `vigem-notify-bus` (DS4 raw output) so they are easy to
  find in a debugger.
- **DualShock 4 `wait_for_ready` could time out on a ready pad.** It needed three neutral reports
  accepted back to back. Each accepted report uses up one read the host has waiting, so a host that
  kept fewer than three reads queued (a slow or busy machine) could reject the third, reset the
  count, and repeat until `ReadyTimeout`. Now one accepted report is enough: the driver accepts a
  report only when the host has a read waiting, so one success already shows the host is reading
  from the pad. Not seen on ViGEmBus 1.21.442 in testing (the host always kept up), but nothing
  relied on the extra two successes.
- **The docs described `TargetNotReady` as a plug-time condition.** They said it happens "right
  after plug" or "when plugging several pads at once", which suggests it stops once the pad is up.
  It doesn't: ViGEmBus drops any report that arrives when the host has no read waiting, at any
  time, and nothing resends it. Ignoring the error on a button release leaves the button stuck. The
  docs (`BusError::TargetNotReady`, `Ready`, the `update` methods, README) now say the report was
  not delivered, when to send it again, and show a short resend loop. No behaviour change.
- **`Client::connect` reported unrelated failures as `VersionMismatch`.** Any failed version check
  except `ERROR_INVALID_PARAMETER` was treated as an incompatible driver, so a timeout or a bus
  device that went away during connect told you to reinstall ViGEmBus. The driver signals a real
  mismatch only with `ERROR_NOT_SUPPORTED`; now only that code gives `VersionMismatch`, and every
  other failure is `BusNotFound` with the actual error as its `source`. A failed interface-size
  lookup during connect is now kept as that `source` too, instead of being dropped.
- **`plug` threw away pads the host was slow to start.** ViGEmBus waits only 1 s for the host to
  start a new pad and then fails `WAIT_DEVICE_READY` with `ERROR_DEVICE_HARDWARE_ERROR`. `plug`
  treated that as fatal, unplugged the pad and returned a `Bus` error, though the pad usually
  becomes usable shortly after (for example on a slow or busy system). Now `plug` keeps the pad
  and `wait_for_ready` decides, as it already did for drivers without `WAIT_DEVICE_READY`. Other
  wait failures still unplug and fail.
- **A timed-out `plug` could leave a pad nobody owned.** If the plug request itself hit this
  crate's 5 s IOCTL timeout, the driver might still have created the pad, but `plug` returned
  without removing it and freed its serial. The pad stayed until the `Client` and all its handles
  were dropped. Now `plug` sends a best-effort unplug for that serial before returning the error.
- **A failed wait on a driver request was reported as a timeout.** If waiting for a ViGEmBus
  request failed outright (not timed out), the error still carried `ERROR_TIMEOUT`, so it looked
  like a slow driver and hid the real cause. Now the error carries the Windows error from the
  failed wait, or `E_UNEXPECTED` if Windows gave none. This should almost never happen in practice.
- Removed the docs claim that using `register_notification` and `register_notification_raw_buffer`
  on one pad "may split host output between them".
- `with_vid` / `with_pid` now document that a `0` for either id makes ViGEmBus ignore both and use
  its defaults for the pad type (`045E:028E` for Xbox 360, `054C:05C4` for DualShock 4). No
  behaviour change.

## [0.2.0]

### In short

**0.2.0 is a rewrite of everything under the hood, and a breaking release for the API on top of it.**
The old version worked, but it was fragile in the places that matter for a long-running app: a
background notification thread could hang forever, every input report allocated and destroyed a
Windows event handle, a slow listener could grow memory without limit, and a pad could report itself
"ready" a moment before the driver was actually willing to accept input. Those are all fixed. On the
API side, the type system now stops you from sending input to a controller that isn't ready yet
(`plug()` gives you a `Pending` handle, `wait_for_ready()` turns it into a `Ready` one), errors are
real variants you can match on and recover from instead of one opaque "Windows error", the DS4
button field is a proper type that can no longer silently wipe your D-pad, and there's a
`TouchpadTracker` so touch gestures work without hand-rolling packet counters. DS4 rumble also had
its two motors swapped; that's fixed, so large and small motors now mean what they say. Upgrading
requires code changes, but they're mechanical; see the [migration cheat sheet](#migration-cheat-sheet)
at the end of this entry.

### Added

**PnP serial discovery**

- **PnP serial discovery.** `SerialDiscovery` (`Auto` default, `Eager`, `Off`) via
  `ClientBuilder::serial_discovery`. After a slot conflict (`Auto`) or before each plug
  (`Eager`), the crate walks children of the opened ViGEmBus device node and skips serials
  already present in the PnP tree. Enumeration failure never fails a plug (degrades to
  randomised retry).
- `Client::occupied_serials()`: machine-wide occupancy snapshot for diagnostics / custom
  placement (lags the driver; not a reservation).
- `Client::serial_discovery()`: policy set at connect time.

**Readiness as a type state**

- `TargetHandle<T, S>` now carries a readiness state: `Pending` (just plugged) or `Ready` (can send
  input). `plug()` returns `Pending`; `update`, `update_ex`, `register_notification*`, and
  `user_index` only exist on `Ready`. `Ready` is the default type parameter, so
  `TargetHandle<Xbox360>` still means "usable".
- `wait_for_ready()` / `wait_for_ready_timeout(Duration)` take `&self` and return a `Ready`
  handle (the `Pending` handle stays usable, so a timeout does not unplug the pad). Default
  timeout is 3 seconds.
- `assume_ready()` skips the probe entirely for callers who accept that early reports may be no-ops.
- `ClientError::ReadyTimeout { timeout }` for a readiness timeout (distinct from
  `BusError::TargetNotReady`).

**Errors you can act on**

- New `error` module, re-exported at the crate root: `ClientError`, `BusError`, `Win32Error`,
  `Operation`, `RecvError`, `TryRecvError`, `RecvTimeoutError`.
- `BusError` variants: `DeviceGone`, `AccessDenied`, `TargetNotReady`, `Ioctl`, `Enumeration`,
  `VersionMismatch { requested }`, `BusNotFound { source }`. Both `BusError` and `ClientError` are
  `#[non_exhaustive]`; `BusError` is `Clone` (notification fan-out delivers one error to every
  subscriber).
- `Operation` records which driver step failed (`PLUGIN_TARGET`, `XUSB_SUBMIT_REPORT`,
  `DS4_AWAIT_OUTPUT_AVAILABLE`, ...) for logs, with stable string labels.
- `Win32Error` wraps a raw Windows status as a plain integer, so the `windows` crate is no longer
  part of the public API. Accessors: `hresult()`, `win32_code()`, `from_win32()`, `from_hresult()`.
- `BusError::operation()` and `BusError::win32_source()` for diagnostics.
- New `ClientError` variants: `InvalidMaxTargets`, `ReadyTimeout { timeout }`, `Poisoned`.

**Client introspection and control**

- `Client` is now `Clone` (`Arc` inside) and implements `Debug`.
- `Client::unplug_all()` removes every pad the client owns while keeping the connection open.
- `Client::max_targets()`, `Client::target_count()`, `Client::live_serials()`
  (returns `BTreeSet<u32>`, same shape as `occupied_serials`).
- `TargetHandle::serial_no()` and a `Debug` impl (includes device type and readiness state).
- `#[must_use]` on `ClientBuilder` and `TargetBuilder` so a forgotten `.connect()` /
  `.plug()` is a compile warning.
- Public constants `DEFAULT_MAX_TARGETS` (16) and `MAX_TARGETS_LIMIT` (64).

**Notification subscriptions**

- `NotificationReceiver<N>` replaces the raw `std::sync::mpsc::Receiver`. Same method
  names (`recv`, `try_recv`, `recv_timeout`, `iter`) plus `capacity()`, `is_bounded()`,
  `len()`, `is_empty()`, and `dropped_count()`. `recv` is a single
  `Result<N, RecvError>` (`Disconnected` / `Bus`), not a nested
  `Result<Result<N, BusError>, mpsc::RecvError>`. Same idea for `try_recv` (`Empty`)
  and `recv_timeout` (`Timeout`). Those error types are crate-owned, not
  `std::sync::mpsc`.
- `NotificationBuffer::{Bounded { capacity }, Unbounded}` and
  `register_notification_with` / `register_notification_raw_buffer_with` to choose a policy per
  subscription.
- `NOTIFICATION_CHANNEL_CAPACITY` (4) exported as the default bounded capacity.
- Multiple subscriptions on the same pad and kind now share **one** worker and **one** pending driver
  request, and every subscriber receives every event.

**DualShock 4 ergonomics**

- `Ds4Buttons`: a typed button word (`repr(transparent)` over `u16`) that keeps the D-pad nibble and
  the face/shoulder flags from overwriting each other. Combinators: `with`, `without`, `with_dpad`,
  `contains`, `dpad`, `flags`, `bits`, `from_bits_retain`, plus `|` / `|=` / `&=` / `-=` with
  `Ds4ButtonFlags`. There is no `BitOr<Ds4Buttons>`: the d-pad is one direction, not flags.
- `TouchPoint`: a validated touchpad coordinate (`new` returns `Option`, `new_clamped` clamps
  explicitly, `MAX_X` = 1919, `MAX_Y` = 942).
- `TouchState::{Down { tracking_num, at }, Up}` and `Ds4Touch::set_finger_1` / `set_finger_2` /
  `finger_1()` / `finger_2()`.
- `TouchpadTracker`: allocates tracking numbers and advances the packet counter for you, so a
  multi-frame drag reads as one continuous gesture on the host.
- `Ds4Report::set_buttons` and `Ds4ReportExData::set_buttons` (replace flags, keep the D-pad).
- `Ds4ReportExData::to_report()` / `set_report()`.
- `Ds4ReportEx::as_bytes()`, `as_bytes_mut()`, `from_bytes()`, `data()`, `data_mut()`.
- Convenience constructors: `Ds4Notification::new`, `Ds4LightbarColor::new` (now `const`),
  `Ds4OutputBuffer::new`, `X360Notification::new`.

**Crate-level**

- `serde` feature (off by default): `Serialize` / `Deserialize` on reports, notifications, and
  related value types. `TouchPoint` rejects out-of-range coordinates on deserialize.
- `windows-error` feature (off by default): `Win32Error::to_windows_error()` and
  `BusError::as_windows_error()` for callers who want the `windows::core::Error` back.
- `ClientError` is `Clone` (every variant already was).
- Reports and notifications now derive `PartialEq`, `Eq`, and `Hash`, so frame-to-frame diffing and
  set membership work.
- `Ds4Touch::default()` (and therefore `Ds4ReportEx::default()`) reports both fingers **up**.
  All-zero tracking bytes mean *down* on the wire; `Default` now sets bit 7 on each slot.
- Declared MSRV: Rust 1.88 (`rust-version` in `Cargo.toml`).
- Non-Windows targets emit a single clear `compile_error!` instead of a wall of unresolved imports.
  Building with neither `x360` nor `ds4` also fails with a clear message.
- Crate lints turned on: `missing_docs`, `missing_debug_implementations`, `unreachable_pub`,
  `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`, and others. Every public item is
  documented.
- docs.rs metadata: feature badges (`doc_cfg`), links to definitions, and the changelog published as
  a `changelog` module. Crate-root rustdoc is the GitHub README. Relative README links (changelog,
  examples, licenses) are absolute GitHub URLs so they work on docs.rs.
  `rustdoc::broken_intra_doc_links` is denied. Crate-root re-exports of the types people land on
  (`Client`, `TargetHandle`, reports, errors) use `#[doc(inline)]`.
- Example `ds4_ex` renamed to `ds4_touch`.
- Test and benchmark suites: `tests/api_surface.rs` (public surface, `Send`/`Sync`),
  `tests/driver_required.rs` (live driver integration, `#[ignore]`), `tests/stress.rs` (churn, soak,
  multi-pad chaos with handle- and memory-leak checks), and `benches/latency.rs` (Criterion,
  per-`update` latency).

### Changed

**PnP serial discovery**

- Plug path consults PnP occupancy when discovery is enabled; serial assignment rules are
  unchanged, only collision frequency. When discovery masks the entire scan window, one
  final attempt bypasses the snapshot so `BusSerialsExhausted` always carries a real driver
  error. Auto walks at most once per plug call (even after a full-window bypass).

**Breaking: DualShock 4 report fields**

- `Ds4Button` renamed to `Ds4ButtonFlags`.
- `Ds4Report::buttons` is now `Ds4Buttons` instead of a raw `u16`.
  `report.buttons = Ds4Button::CROSS.bits()` becomes `report.buttons |= Ds4ButtonFlags::CROSS`.
- `Ds4Report::special` is now `Ds4SpecialButton` instead of a raw `u8`.
- `Ds4Report::trigger_l` / `trigger_r` renamed to `trigger_left` / `trigger_right` (same for
  `Ds4ReportExData`).
- `Ds4ReportEx` changed from a `union` to a `struct` with `Deref`/`DerefMut` to `Ds4ReportExData`.
  Field access is unchanged (`report_ex.gyro_x = ...`); the raw-buffer view moved to `as_bytes()` /
  `from_bytes()`.
- `Ds4ReportExData::as_report()` / `as_report_mut()` replaced by `to_report()` / `set_report()`.
  The old versions handed out a reference to a `align(2)` type inside a `packed` struct, which is
  not sound; the new ones copy by value.
- `Ds4ReportExData::_unknown1` / `_unknown2` are now private reserved fields, so struct-update syntax
  from outside the crate no longer works. Start from `Default` instead.
- `Ds4Touch` getters lost their `get_` prefix: `is_down_1`, `tracking_num_1`, `coords_1` (and the
  `_2` equivalents). `get_packet_counter()` is gone: `packet_counter` was already a public field.

**Breaking: client and target API**

- `TargetHandle<T>` is now `TargetHandle<T, S = Ready>`.
- `register_notification()` returns `NotificationReceiver<N>` instead of
  `Receiver<Result<N, BusError>>`. `recv` / `try_recv` / `recv_timeout` return
  `Result<N, RecvError>` (and the try/timeout counterparts), not
  `Result<Result<N, BusError>, mpsc::…>`.
- `ClientError::TargetDoesNotExist(u32)` and `NoFreeSlot` became struct variants
  (`TargetDoesNotExist { serial_no }`, `NoFreeSlot { max_targets }`).
- `ClientError::BusError(..)` renamed to `ClientError::Bus(..)`, and its `Display` is now transparent:
  you see the actual driver problem instead of a generic wrapper line.
- `max_targets` is validated: `0` or anything above `MAX_TARGETS_LIMIT` (64) returns
  `ClientError::InvalidMaxTargets` before any driver call, rather than being silently accepted.
- A poisoned internal mutex returns `ClientError::Poisoned` instead of panicking.
- `TargetBuilder::plugin()` renamed to `plug()` (verb; pairs with `unplug()`). A
  `#[deprecated]` `plugin()` alias remains for 0.1 copy-paste.
- `TargetHandle::get_user_index()` renamed to `user_index()`. A `#[deprecated]`
  `get_user_index()` alias remains.
- `Ds4OutputBuffer.buf` renamed to `bytes`.

**Behavioral**

- **Readiness detection rewritten.** 0.1.1 started a notification thread and waited for 250 ms of
  silence, which is a proxy signal, not a real one. 0.2.0 sends neutral input reports until the
  driver accepts **three in a row** (a single success can still be followed by a rejection under
  concurrent multi-pad plug). `wait_for_ready_timeout(Duration::ZERO)` is an explicit single probe.
- **One overlapped call per handle.** Every `update` in 0.1.1 called `CreateEventW` and
  `CloseHandle`. 0.2.0 caches one overlapped call per pad and reuses it, so the hot path does no
  handle churn. Concurrent `update`s on one pad serialize on that call (one IRP in flight);
  cloning a handle shares the pad, it does not parallelise throughput.
- **All driver waits are bounded.** Blocking IOCTLs time out (5 s normally, 1 s from destructors),
  and a cancelled request is drained with a bounded wait (2 s) before its resources are released.
  Nothing waits forever any more.
- **Notification workers are cancelled and joined.** Dropping the last receiver, unplugging the pad,
  or dropping the client cancels the pending driver request and joins the thread.
- **Notification buffers are bounded by default** (capacity 4, drop-oldest), so a slow consumer
  drops its own events instead of growing memory or stalling the worker. Opt into
  `NotificationBuffer::Unbounded` if you need every event.
- **Plug no longer holds the client lock across the driver call.** Serials are reserved under the
  lock, the IOCTL runs unlocked, and slot conflicts (`ERROR_BUSY` / `ERROR_ALREADY_EXISTS` /
  `ERROR_INVALID_PARAMETER`) retry the next serial. ViGEmBus maps an already-taken serial to
  `ERROR_INVALID_PARAMETER`, so that code must be retried for multi-process clients. Other failures
  are returned as-is instead of being reported as "no free slot".
- **Pads have a generation number.** Serials are reused after unplug; a stale handle from a previous
  generation can no longer drive whatever pad now occupies that slot. The same flag stops a stale
  notification receiver from observing another pad's rumble or LED data.
- **`is_attached()` distinguishes cases.** `Ok(false)` means the pad was unplugged;
  `Err(ClientError::ClientNoLongerExists)` means the owning client is gone.
- **`Ds4ReportExData::default()` no longer uses `mem::zeroed()`.** Every field is explicitly
  initialised.
- **Wire buffers are zeroed before use.** Report structs are built from an all-zero base so
  `repr(C)` padding bytes are defined before being copied into the kernel (matching upstream ViGEm's
  `*_INIT` helpers).
- Missing `IOCTL_VIGEM_WAIT_DEVICE_READY` support on older drivers is treated as non-fatal instead
  of failing the plug.
- `user_index()` now checks that the pad is still live before calling the driver.
- Dependencies: `bitflags` 2.10 -> 2.13. The `windows` crate is now target-gated to
  `cfg(windows)` and is a private dependency unless you enable `windows-error`.
- Examples declare `required-features`, so `cargo run --example ds4 --no-default-features` no longer
  fails to compile.

### Fixed

- **DS4 rumble motors were swapped.** The wire layout of ViGEmBus's `DS4_OUTPUT_REPORT` is *small
  motor, then large motor*, the opposite of `DS4_OUTPUT_DATA` in the client headers. 0.1.1 read it
  the wrong way round, so `Ds4Notification::large_motor` actually carried the small motor's value
  and vice versa. If you compensated for this in your own code, remove the workaround.
- **Notification threads could hang forever.** The worker blocked in `GetOverlappedResult(..., true)`
  with no cancellation, so dropping the receiver did not end the thread until the host happened to
  send something. Workers are now cancellable and joined.
- **Device enumeration could loop forever.** A persistent (non-`ERROR_NO_MORE_ITEMS`) failure while
  enumerating interfaces re-queried the same index indefinitely, wedging `Client::connect()`. The
  iterator now stops permanently after end-of-list or a hard error.
- **A version mismatch was silently swallowed.** `connect()` skipped an incompatible device and
  eventually reported "bus not found". Connect failures are now classified: access denied dominates
  (it's actionable), then version mismatch, then not-found.
- **Unbounded notification queues grew without limit** when the consumer fell behind.
- **Unsound references into packed structs.** `as_report()` / `as_report_mut()` produced
  `&Ds4Report` (alignment 2) pointing into a `repr(C, packed)` container.
- **`Ds4Report::buttons` could be wiped by accident.** Assigning `Ds4Button::X.bits()` cleared the
  D-pad nibble; `set_dpad` before a button assignment silently lost its effect. `Ds4Buttons` makes
  that impossible with the provided combinators.
- **Touch coordinates were clamped silently.** Out-of-range values are now either rejected
  (`TouchPoint::new`) or clamped by explicit opt-in (`TouchPoint::new_clamped`).
- **A poisoned client mutex panicked** via `.expect("Client mutex was poisoned")` on hot paths.
- **A pad could leak on the bus** if the driver plug succeeded but the client failed to record it;
  the plug is now unwound on that path.
- **Short driver transfers were accepted as valid completions.** Notification results are now
  validated against the expected wire size, with a bounded transient-retry budget (8 consecutive
  failures) before the subscription ends.
- 32-bit builds: `SP_DEVICE_INTERFACE_DETAIL_DATA_W::cbSize` uses the C packing value (6 on x86, 8
  on x64) rather than Rust's `size_of`, which failed with `ERROR_INVALID_USER_BUFFER` on i686.

### Deprecated

- `Ds4Touch::set_touch_1` and `Ds4Touch::set_touch_2`. Use `set_finger_1` / `set_finger_2` with
  `TouchState`, or let `TouchpadTracker` handle tracking numbers. The old methods still work and
  keep their original behaviour.
- `TargetBuilder::plugin()` — use `plug()`.
- `TargetHandle::get_user_index()` — use `user_index()`.

### Removed

- `ClientError::WindowsAPIError` and `BusError::WindowsAPIError`. Match the classified variants
  instead; the raw status is available via `BusError::win32_source()`.
- `Ds4Button` (renamed; see Changed).
- `Ds4Touch::get_packet_counter()`, `get_is_down_1/2()`, `get_tracking_num_1/2()`,
  `get_coords_1/2()` (renamed; see Changed).
- `Ds4ReportExData::as_report()` / `as_report_mut()` (replaced; see Changed).
- Public access to `Ds4ReportExData::_unknown1` / `_unknown2`.
- The `Ds4ReportEx` union representation, including `report` / `report_buffer` field access.

### Migration cheat sheet

| 0.1.1 | 0.2.0 |
|---|---|
| `let pad = client.new_x360_target().plugin()?;`<br>`pad.wait_for_ready()?;` | `let pad = client.new_x360_target().plug()?.wait_for_ready()?;` |
| `pad.get_user_index()` | `pad.user_index()` |
| `buffer.buf` | `buffer.bytes` |
| `Ds4Button::CROSS` | `Ds4ButtonFlags::CROSS` |
| `report.buttons = Ds4Button::CROSS.bits();` | `report.buttons \|= Ds4ButtonFlags::CROSS;` |
| `report.buttons & Ds4Button::CROSS.bits() != 0` | `report.buttons.contains(Ds4ButtonFlags::CROSS)` |
| `report.special = 0x01;` | `report.special \|= Ds4SpecialButton::PS;` |
| `report.trigger_l` / `report.trigger_r` | `report.trigger_left` / `report.trigger_right` |
| `touch.set_touch_1(true, id, x, y);` | `touch.set_finger_1(TouchState::Down { tracking_num: id, at: TouchPoint::new(x, y).unwrap() });` |
| manual `packet_counter` bookkeeping | `TouchpadTracker::apply(&mut report_ex, finger_1, finger_2)` |
| `touch.get_coords_1()` | `touch.coords_1()` |
| `report_ex.as_report_mut().set_dpad(d);` | `report_ex.set_dpad(d);` |
| `Err(ClientError::TargetDoesNotExist(n))` | `Err(ClientError::TargetDoesNotExist { serial_no: n })` |
| `Err(ClientError::BusError(e))` | `Err(ClientError::Bus(e))` |
| `Err(ClientError::WindowsAPIError(e))` | match `ClientError::Bus(BusError::...)`; raw status via `win32_source()` |
| `let rx: Receiver<_> = pad.register_notification()?;` | `let rx: NotificationReceiver<_> = pad.register_notification()?;` |
| `while let Ok(Ok(n)) = rx.recv()` | `while let Ok(n) = rx.recv()` |
| `Err(std::sync::mpsc::RecvTimeoutError::…)` | `Err(vigem_rust::RecvTimeoutError::…)` |

**Note on DS4 rumble:** if your code compensated for the swapped motors, delete the workaround.
`large_motor` and `small_motor` now report the correct values.

---

## [0.1.1]

### Fixed

- docs.rs builds: set `[package.metadata.docs.rs]` so documentation is generated for the Windows
  target (`x86_64-pc-windows-msvc`) with all features enabled.

## [0.1.0]

Initial release.
