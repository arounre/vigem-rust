//! Demultiplexes `IOCTL_DS4_AWAIT_OUTPUT_AVAILABLE` by serial.
//!
//! The driver ignores the serial in that request: it parks every pending
//! request in one bus-wide queue per file handle and hands each DS4 output
//! report (from any pad, any process) to one of them, stamped with the
//! originating pad's serial. So there is exactly one await-output worker per
//! bus handle, and it routes each completion to the pad it names. Reports for
//! serials with no route (other pads, other processes) are dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::controller::ds4::Ds4OutputBuffer;
use crate::error::BusError;
use crate::internal::bus::{NotificationSink, NotificationWorkerHandle};
use crate::internal::notification_hub::{Fanout, SharedFanout};

/// One completed await-output request: originating serial and the report.
pub(crate) type RawOutput = (u32, Ds4OutputBuffer);

struct Route {
    /// The pad's own liveness flag. Identity (not just the serial) decides
    /// whether a route belongs to the current pad on a recycled serial.
    alive: Arc<AtomicBool>,
    fanout: Arc<Fanout<Ds4OutputBuffer>>,
}

#[derive(Default)]
struct RoutesState {
    routes: HashMap<u32, Route>,
    /// Worker exited or is exiting; no new routes.
    closed: bool,
}

/// Routing table shared by the router and its worker thread.
#[derive(Default)]
pub(crate) struct RouterCore {
    state: Mutex<RoutesState>,
}

impl RouterCore {
    fn lock(&self) -> MutexGuard<'_, RoutesState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add or replace the route for `serial_no`. `false` if the router is closed.
    ///
    /// A route left by a previous pad on the same serial (not yet swept) is
    /// closed and replaced.
    pub(crate) fn insert(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<Ds4OutputBuffer>>,
    ) -> bool {
        let mut st = self.lock();
        if st.closed {
            return false;
        }
        if let Some(old) = st.routes.insert(serial_no, Route { alive, fanout }) {
            old.fanout.close();
        }
        true
    }

    /// Remove `serial_no`'s route if it is still the one using `fanout`.
    fn remove(&self, serial_no: u32, fanout: &Arc<Fanout<Ds4OutputBuffer>>) {
        let mut st = self.lock();
        if st
            .routes
            .get(&serial_no)
            .is_some_and(|r| Arc::ptr_eq(&r.fanout, fanout))
        {
            st.routes.remove(&serial_no);
        }
        drop(st);
        fanout.close();
    }

    /// Close and drop every route whose pad is gone.
    fn sweep(st: &mut RoutesState) {
        st.routes.retain(|_, r| {
            let keep = r.alive.load(Ordering::Acquire);
            if !keep {
                r.fanout.close();
            }
            keep
        });
    }

    #[cfg(test)]
    fn route_count(&self) -> usize {
        self.lock().routes.len()
    }
}

impl NotificationSink<RawOutput> for RouterCore {
    fn deliver(&self, item: Result<RawOutput, BusError>) -> bool {
        let mut st = self.lock();
        if st.closed {
            return false;
        }
        match item {
            Ok((serial_no, buffer)) => {
                let Some(route) = st.routes.get(&serial_no) else {
                    // Another pad's report (ours unsubscribed, or another process).
                    return true;
                };
                if route.alive.load(Ordering::Acquire) {
                    let _ = route.fanout.deliver(Ok(buffer));
                } else if let Some(dead) = st.routes.remove(&serial_no) {
                    dead.fanout.close();
                }
            }
            Err(e) => {
                for route in st.routes.values() {
                    let _ = route.fanout.deliver(Err(e.clone()));
                }
            }
        }
        true
    }

    /// Sweeps dead pads. Stops once no route is left: the router is then
    /// unreachable for new routes and a later registration starts a new one.
    fn should_stop(&self) -> bool {
        let mut st = self.lock();
        Self::sweep(&mut st);
        if st.routes.is_empty() {
            st.closed = true;
        }
        st.closed
    }

    fn close(&self) {
        let mut st = self.lock();
        st.closed = true;
        for (_, route) in st.routes.drain() {
            route.fanout.close();
        }
    }
}

/// The single await-output worker for one bus handle plus its routing table.
///
/// Held by every [`Ds4OutputRoute`] on this bus; dropping the last one
/// cancels and joins the worker.
pub(crate) struct Ds4OutputRouter {
    core: Arc<RouterCore>,
    /// `Mutex` so this type is `Sync` (`JoinHandle` is not).
    worker: Mutex<Option<NotificationWorkerHandle>>,
}

impl Ds4OutputRouter {
    pub(crate) fn new(core: Arc<RouterCore>, worker: NotificationWorkerHandle) -> Self {
        Self {
            core,
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Add a route; `false` if this router is closed (caller starts a new one).
    pub(crate) fn add_route(
        &self,
        serial_no: u32,
        alive: Arc<AtomicBool>,
        fanout: Arc<Fanout<Ds4OutputBuffer>>,
    ) -> bool {
        self.core.insert(serial_no, alive, fanout)
    }
}

impl Drop for Ds4OutputRouter {
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Cancels the IRP and joins; the worker closes the core on exit.
        drop(worker);
        self.core.close();
    }
}

/// One pad's raw-output subscription point. Shared by all of that pad's raw
/// receivers; dropping the last one removes the pad's route.
pub(crate) struct Ds4OutputRoute {
    fanout: Arc<Fanout<Ds4OutputBuffer>>,
    serial_no: u32,
    router: Arc<Ds4OutputRouter>,
}

impl Ds4OutputRoute {
    pub(crate) fn new(
        fanout: Arc<Fanout<Ds4OutputBuffer>>,
        serial_no: u32,
        router: Arc<Ds4OutputRouter>,
    ) -> Self {
        Self {
            fanout,
            serial_no,
            router,
        }
    }
}

impl SharedFanout<Ds4OutputBuffer> for Ds4OutputRoute {
    fn fanout(&self) -> &Arc<Fanout<Ds4OutputBuffer>> {
        &self.fanout
    }
}

impl Drop for Ds4OutputRoute {
    fn drop(&mut self) {
        // Lock released before `router` drops, so a last-router drop (which
        // joins the worker) never runs under the routes lock.
        self.router.core.remove(self.serial_no, &self.fanout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NotificationBuffer;
    use crate::error::Operation;
    use crate::internal::bounded_channel::NotifReceiver;
    use std::sync::mpsc::TryRecvError;

    fn buf(tag: u8) -> Ds4OutputBuffer {
        Ds4OutputBuffer::new([tag; 64])
    }

    type Rx = NotifReceiver<Result<Ds4OutputBuffer, BusError>>;

    fn route(
        core: &RouterCore,
        serial: u32,
    ) -> (Arc<AtomicBool>, Arc<Fanout<Ds4OutputBuffer>>, Rx) {
        let alive = Arc::new(AtomicBool::new(true));
        let fanout = Arc::new(Fanout::new());
        let (_id, rx) = fanout
            .subscribe(NotificationBuffer::Unbounded)
            .expect("fresh fanout");
        assert!(core.insert(serial, Arc::clone(&alive), Arc::clone(&fanout)));
        (alive, fanout, rx)
    }

    fn tag(rx: &Rx) -> u8 {
        rx.try_recv().expect("item").expect("ok").bytes[0]
    }

    #[test]
    fn routes_by_serial_and_drops_unknown() {
        let core = RouterCore::default();
        let (_a1, _f1, rx1) = route(&core, 1);
        let (_a2, _f2, rx2) = route(&core, 2);

        assert!(core.deliver(Ok((1, buf(10)))));
        assert!(core.deliver(Ok((2, buf(20)))));
        assert!(core.deliver(Ok((99, buf(99)))));
        assert!(core.deliver(Ok((1, buf(11)))));

        assert_eq!(tag(&rx1), 10);
        assert_eq!(tag(&rx1), 11);
        assert!(matches!(rx1.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(tag(&rx2), 20);
        assert!(matches!(rx2.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn dead_pad_route_is_closed_and_removed() {
        let core = RouterCore::default();
        let (a1, _f1, rx1) = route(&core, 1);
        let (_a2, _f2, rx2) = route(&core, 2);

        a1.store(false, Ordering::Release);
        assert!(!core.should_stop(), "pad 2 still routed");
        assert_eq!(core.route_count(), 1);
        assert!(matches!(rx1.try_recv(), Err(TryRecvError::Disconnected)));

        assert!(core.deliver(Ok((2, buf(2)))));
        assert_eq!(tag(&rx2), 2);
    }

    #[test]
    fn dead_pad_seen_on_delivery_before_sweep() {
        let core = RouterCore::default();
        let (a1, _f1, rx1) = route(&core, 1);
        let (_a2, _f2, _rx2) = route(&core, 2);

        a1.store(false, Ordering::Release);
        assert!(core.deliver(Ok((1, buf(1)))));
        assert!(matches!(rx1.try_recv(), Err(TryRecvError::Disconnected)));
        assert_eq!(core.route_count(), 1);
    }

    #[test]
    fn recycled_serial_replaces_stale_route() {
        let core = RouterCore::default();
        let (old_alive, old_fanout, old_rx) = route(&core, 1);
        old_alive.store(false, Ordering::Release);

        let (_new_alive, _new_fanout, new_rx) = route(&core, 1);
        assert!(matches!(old_rx.try_recv(), Err(TryRecvError::Disconnected)));

        // Old route's late removal must not unhook the new pad.
        core.remove(1, &old_fanout);
        assert_eq!(core.route_count(), 1);
        assert!(core.deliver(Ok((1, buf(7)))));
        assert_eq!(tag(&new_rx), 7);
    }

    #[test]
    fn last_route_removed_stops_worker_and_rejects_new_routes() {
        let core = RouterCore::default();
        let (_a1, f1, _rx1) = route(&core, 1);
        let (_a2, f2, _rx2) = route(&core, 2);

        core.remove(1, &f1);
        assert!(!core.should_stop());
        core.remove(2, &f2);
        assert!(core.should_stop(), "no routes left: worker must stop");

        let fanout = Arc::new(Fanout::new());
        assert!(
            !core.insert(3, Arc::new(AtomicBool::new(true)), fanout),
            "closed router must refuse routes so the caller starts a new one"
        );
    }

    #[test]
    fn bus_error_reaches_every_route_then_close_disconnects() {
        let core = RouterCore::default();
        let (_a1, _f1, rx1) = route(&core, 1);
        let (_a2, _f2, rx2) = route(&core, 2);

        let err = BusError::DeviceGone {
            operation: Operation::DS4_AWAIT_OUTPUT_AVAILABLE,
            serial_no: None,
        };
        assert!(core.deliver(Err(err)));
        core.close();

        for rx in [&rx1, &rx2] {
            assert!(matches!(
                rx.try_recv(),
                Ok(Err(BusError::DeviceGone { .. }))
            ));
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
        }
        assert!(
            !core.deliver(Ok((1, buf(1)))),
            "closed core stops the worker"
        );
    }
}
