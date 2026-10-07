//! Background host probes owned by one launcher invocation.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use stargaze_core::config::HostEntry;
use stargaze_core::status::{self, ServerStatus};
use tokio::sync::oneshot;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
type Endpoint = (String, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostStatus {
    Checking,
    Unreachable,
    Responding(ServerStatus),
}

impl HostStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Checking => "Checking…",
            Self::Unreachable => "Stopped / unreachable",
            Self::Responding(ServerStatus::Starting) => "Starting",
            Self::Responding(ServerStatus::Started) => "Started",
            Self::Responding(ServerStatus::Stopping) => "Stopping",
        }
    }
}

struct Pending {
    rx: oneshot::Receiver<HostStatus>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Probe {
    status: HostStatus,
    next_poll: Instant,
    pending: Option<Pending>,
}

impl Probe {
    fn new() -> Self {
        Self {
            status: HostStatus::Checking,
            next_poll: Instant::now(),
            pending: None,
        }
    }

    fn collect(&mut self) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(status) => self.status = status,
            Err(oneshot::error::TryRecvError::Closed) => self.status = HostStatus::Unreachable,
            Err(oneshot::error::TryRecvError::Empty) => return,
        }
        self.pending = None;
    }
}

/// Endpoint-keyed results cannot move to another host when rows are edited
/// or deleted. Dropping this collection aborts every in-flight probe.
#[derive(Default)]
pub(super) struct HostStatuses {
    probes: HashMap<Endpoint, Probe>,
}

impl HostStatuses {
    fn reconcile(&mut self, hosts: &[HostEntry]) {
        let endpoints: HashSet<_> = hosts.iter().map(endpoint).collect();
        self.probes.retain(|key, _| endpoints.contains(key));
        for key in endpoints {
            self.probes.entry(key).or_insert_with(Probe::new);
        }
    }

    pub(super) fn tick(&mut self, hosts: &[HostEntry], rt: &tokio::runtime::Handle) {
        self.reconcile(hosts);
        let now = Instant::now();
        for (key, probe) in &mut self.probes {
            probe.collect();
            if probe.pending.is_none() && now >= probe.next_poll {
                let (address, port) = key.clone();
                let (tx, rx) = oneshot::channel();
                let task = rt.spawn(async move {
                    let result = match status::probe(&address, port).await {
                        Ok(status) => HostStatus::Responding(status),
                        Err(_) => HostStatus::Unreachable,
                    };
                    let _ = tx.send(result);
                });
                probe.pending = Some(Pending { rx, task });
                probe.next_poll = now + POLL_INTERVAL;
            }
        }
    }

    pub(super) fn get(&self, host: &HostEntry) -> HostStatus {
        self.probes
            .get(&endpoint(host))
            .map_or(HostStatus::Checking, |probe| probe.status)
    }
}

fn endpoint(host: &HostEntry) -> Endpoint {
    (host.address.clone(), host.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(address: &str, port: u16) -> HostEntry {
        HostEntry {
            address: address.into(),
            port,
            ..HostEntry::default()
        }
    }

    #[test]
    fn reorder_delete_and_rename_preserve_only_matching_endpoints() {
        let first = host("host-a", 9000);
        let mut second = host("host-b", 9000);
        let mut statuses = HostStatuses::default();
        statuses.reconcile(&[first.clone(), second.clone()]);
        statuses.probes.get_mut(&endpoint(&second)).unwrap().status =
            HostStatus::Responding(ServerStatus::Started);
        second.name = "Renamed".into();
        statuses.reconcile(&[second.clone(), first.clone()]);
        assert_eq!(
            statuses.get(&second),
            HostStatus::Responding(ServerStatus::Started)
        );
        statuses.reconcile(std::slice::from_ref(&second));
        assert_eq!(statuses.probes.len(), 1);
        assert_eq!(statuses.get(&first), HostStatus::Checking);
        second.port = 9002;
        statuses.reconcile(std::slice::from_ref(&second));
        assert_eq!(statuses.get(&second), HostStatus::Checking);
    }

    #[test]
    fn duplicate_hosts_share_one_probe() {
        let host = host("same", 9000);
        let mut statuses = HostStatuses::default();
        statuses.reconcile(&[host.clone(), host]);
        assert_eq!(statuses.probes.len(), 1);
    }

    #[tokio::test]
    async fn edited_endpoint_cancels_probe_and_discards_stale_response() {
        let mut host = host("old", 9000);
        let mut statuses = HostStatuses::default();
        statuses.reconcile(std::slice::from_ref(&host));
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(std::future::pending());
        let aborted = task.abort_handle();
        statuses.probes.get_mut(&endpoint(&host)).unwrap().pending = Some(Pending { rx, task });
        host.address = "new".into();
        statuses.reconcile(std::slice::from_ref(&host));
        assert!(
            tx.send(HostStatus::Responding(ServerStatus::Started))
                .is_err()
        );
        assert_eq!(statuses.get(&host), HostStatus::Checking);
        tokio::task::yield_now().await;
        assert!(aborted.is_finished());
    }

    #[tokio::test]
    async fn polling_tracks_lifecycle_and_disconnect_over_tcp() {
        let listener = stargaze_core::status::StatusListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let host = host("127.0.0.1", listener.local_addr().port());
        let mut statuses = HostStatuses::default();
        statuses.reconcile(std::slice::from_ref(&host));
        assert_eq!(statuses.get(&host), HostStatus::Checking);
        for status in [
            ServerStatus::Starting,
            ServerStatus::Started,
            ServerStatus::Stopping,
        ] {
            listener.set_status(status);
            statuses.probes.get_mut(&endpoint(&host)).unwrap().next_poll = Instant::now();
            wait_for_probe(&mut statuses, &host).await;
            assert_eq!(statuses.get(&host), HostStatus::Responding(status));
        }
        drop(listener);
        tokio::task::yield_now().await;
        statuses.probes.get_mut(&endpoint(&host)).unwrap().next_poll = Instant::now();
        wait_for_probe(&mut statuses, &host).await;
        assert_eq!(statuses.get(&host), HostStatus::Unreachable);
        // A later restart must recover, rather than retaining the failure.
        let _restarted = stargaze_core::status::StatusListener::bind(
            format!("127.0.0.1:{}", host.port).parse().unwrap(),
        )
        .await
        .unwrap();
        statuses.probes.get_mut(&endpoint(&host)).unwrap().next_poll = Instant::now();
        wait_for_probe(&mut statuses, &host).await;
        assert_eq!(
            statuses.get(&host),
            HostStatus::Responding(ServerStatus::Starting)
        );
    }

    async fn wait_for_probe(statuses: &mut HostStatuses, host: &HostEntry) {
        statuses.tick(
            std::slice::from_ref(host),
            &tokio::runtime::Handle::current(),
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                tokio::task::yield_now().await;
                let probe = statuses.probes.get_mut(&endpoint(host)).unwrap();
                probe.collect();
                if probe.pending.is_none() {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn launcher_exit_aborts_pending_probes() {
        let host = host("host", 9000);
        let mut statuses = HostStatuses::default();
        statuses.reconcile(std::slice::from_ref(&host));
        let (_tx, rx) = oneshot::channel();
        let task = tokio::spawn(std::future::pending());
        let aborted = task.abort_handle();
        statuses.probes.get_mut(&endpoint(&host)).unwrap().pending = Some(Pending { rx, task });
        drop(statuses);
        tokio::task::yield_now().await;
        assert!(aborted.is_finished());
    }

    #[tokio::test]
    async fn collects_results_and_does_not_overlap_or_repeat_early() {
        let host = host("host", 9000);
        let mut statuses = HostStatuses::default();
        statuses.reconcile(std::slice::from_ref(&host));
        let probe = statuses.probes.get_mut(&endpoint(&host)).unwrap();
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(std::future::pending());
        let aborted = task.abort_handle();
        probe.pending = Some(Pending { rx, task });
        // Even if the next polling time passes, an in-flight task is retained.
        statuses.tick(
            std::slice::from_ref(&host),
            &tokio::runtime::Handle::current(),
        );
        assert!(!aborted.is_finished());
        let probe = statuses.probes.get_mut(&endpoint(&host)).unwrap();
        probe.next_poll = Instant::now() + POLL_INTERVAL;
        tx.send(HostStatus::Responding(ServerStatus::Stopping))
            .unwrap();
        statuses.tick(
            std::slice::from_ref(&host),
            &tokio::runtime::Handle::current(),
        );
        assert_eq!(
            statuses.get(&host),
            HostStatus::Responding(ServerStatus::Stopping)
        );
        assert!(
            statuses
                .probes
                .get(&endpoint(&host))
                .unwrap()
                .pending
                .is_none()
        );
    }
}
