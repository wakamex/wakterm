use crate::sessionhandler::{PduSender, QueuedPdu, RenderBatch, RenderBatchSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::Pdu;
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use wakterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

enum ReadyItem {
    Notif(MuxNotification),
    WritePdu(QueuedPdu),
    WriteRender(RenderBatch),
    Readable,
}

impl ReadyItem {
    fn lane(&self) -> usize {
        match self {
            Self::Readable => 0,
            Self::WritePdu(_) => 1,
            Self::WriteRender(_) => 2,
            Self::Notif(_) => 3,
        }
    }
}

const MAX_INFLIGHT_CONTROL_REQUESTS: usize = 64;
const CONTROL_REPLY_QUEUE_CAPACITY: usize = MAX_INFLIGHT_CONTROL_REQUESTS;
const RENDER_BATCH_QUEUE_CAPACITY: usize = 1;
const NOTIFICATION_QUEUE_CAPACITY: usize = 256;

#[derive(Clone, PartialEq, Eq, Hash)]
enum AlertStateKey {
    CurrentWorkingDirectory,
    IconTitle,
    WindowTitle,
    TabTitle,
    Palette,
    UserVar(String),
    OutputSinceFocusLost,
    Progress,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum NotificationKey {
    PaneOutput(mux::pane::PaneId),
    Alert(mux::pane::PaneId, AlertStateKey),
    WindowWorkspace(mux::window::WindowId),
    PaneFocus,
    TabResized(mux::tab::TabId),
    TabOrder(mux::window::WindowId),
    ParkedTabs(mux::window::WindowId),
    TabTitle(mux::tab::TabId),
    WindowTitle(mux::window::WindowId),
}

fn alert_state_key(alert: &wakterm_term::Alert) -> Option<AlertStateKey> {
    use wakterm_term::Alert;
    match alert {
        Alert::Bell | Alert::ToastNotification { .. } => None,
        Alert::CurrentWorkingDirectoryChanged => Some(AlertStateKey::CurrentWorkingDirectory),
        Alert::IconTitleChanged(_) => Some(AlertStateKey::IconTitle),
        Alert::WindowTitleChanged(_) => Some(AlertStateKey::WindowTitle),
        Alert::TabTitleChanged(_) => Some(AlertStateKey::TabTitle),
        Alert::PaletteChanged => Some(AlertStateKey::Palette),
        Alert::SetUserVar { name, .. } => Some(AlertStateKey::UserVar(name.clone())),
        Alert::OutputSinceFocusLost => Some(AlertStateKey::OutputSinceFocusLost),
        Alert::Progress(_) => Some(AlertStateKey::Progress),
    }
}

fn notification_key(notification: &MuxNotification) -> Option<NotificationKey> {
    match notification {
        MuxNotification::PaneOutput(pane_id) => Some(NotificationKey::PaneOutput(*pane_id)),
        MuxNotification::Alert { pane_id, alert } => {
            alert_state_key(alert).map(|key| NotificationKey::Alert(*pane_id, key))
        }
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            Some(NotificationKey::WindowWorkspace(*window_id))
        }
        MuxNotification::PaneFocused(_) => Some(NotificationKey::PaneFocus),
        MuxNotification::TabResized { tab_id, .. } => Some(NotificationKey::TabResized(*tab_id)),
        MuxNotification::TabOrderChanged { window_id, .. } => {
            Some(NotificationKey::TabOrder(*window_id))
        }
        MuxNotification::ParkedTabsChanged { window_id, .. } => {
            Some(NotificationKey::ParkedTabs(*window_id))
        }
        MuxNotification::TabTitleChanged { tab_id, .. } => Some(NotificationKey::TabTitle(*tab_id)),
        MuxNotification::WindowTitleChanged { window_id, .. } => {
            Some(NotificationKey::WindowTitle(*window_id))
        }
        _ => None,
    }
}

fn notification_is_forwarded(notification: &MuxNotification) -> bool {
    !matches!(
        notification,
        MuxNotification::PaneAdded(_)
            | MuxNotification::WindowCreated(_)
            | MuxNotification::WindowRemoved(_)
            | MuxNotification::WindowInvalidated(_)
            | MuxNotification::ActiveWorkspaceChanged(_)
            | MuxNotification::Empty
            | MuxNotification::SaveToDownloads { .. }
    )
}

fn notification_kind(notification: &MuxNotification) -> &'static str {
    match notification {
        MuxNotification::PaneOutput(_) => "pane_output",
        MuxNotification::PaneAdded(_) => "pane_added",
        MuxNotification::PaneRemoved(_) => "pane_removed",
        MuxNotification::WindowCreated(_) => "window_created",
        MuxNotification::WindowRemoved(_) => "window_removed",
        MuxNotification::WindowInvalidated(_) => "window_invalidated",
        MuxNotification::WindowWorkspaceChanged(_) => "window_workspace_changed",
        MuxNotification::ActiveWorkspaceChanged(_) => "active_workspace_changed",
        MuxNotification::Alert { .. } => "alert",
        MuxNotification::Empty => "empty",
        MuxNotification::AssignClipboard { .. } => "assign_clipboard",
        MuxNotification::SaveToDownloads { .. } => "save_to_downloads",
        MuxNotification::TabAddedToWindow { .. } => "tab_added_to_window",
        MuxNotification::PaneFocused(_) => "pane_focused",
        MuxNotification::TabResized { .. } => "tab_resized",
        MuxNotification::TabOrderChanged { .. } => "tab_order_changed",
        MuxNotification::ParkedTabsChanged { .. } => "parked_tabs_changed",
        MuxNotification::TabTitleChanged { .. } => "tab_title_changed",
        MuxNotification::WindowTitleChanged { .. } => "window_title_changed",
        MuxNotification::WorkspaceRenamed { .. } => "workspace_renamed",
    }
}

#[derive(Default)]
struct NotificationQueueState {
    queue: VecDeque<MuxNotification>,
    pending_state: HashSet<NotificationKey>,
    closed: bool,
}

struct NotificationQueue {
    state: Mutex<NotificationQueueState>,
    wake_tx: smol::channel::Sender<()>,
    wake_rx: smol::channel::Receiver<()>,
}

impl NotificationQueue {
    fn new() -> Arc<Self> {
        let (wake_tx, wake_rx) = smol::channel::bounded(1);
        Arc::new(Self {
            state: Mutex::new(NotificationQueueState::default()),
            wake_tx,
            wake_rx,
        })
    }

    fn push(&self, notification: MuxNotification) -> bool {
        if !notification_is_forwarded(&notification) {
            metrics::counter!("mux_server.notification.ignored_before_queue").increment(1);
            return true;
        }

        let mut state = self.state.lock().unwrap();
        if state.closed {
            return false;
        }

        let key = notification_key(&notification);
        if let Some(key) = key.as_ref() {
            if state.pending_state.contains(key) {
                if let Some(pending) = state
                    .queue
                    .iter_mut()
                    .find(|pending| notification_key(pending).as_ref() == Some(key))
                {
                    *pending = notification;
                }
                metrics::counter!("mux_server.notification.coalesced_state").increment(1);
                return true;
            }
        }

        if state.queue.len() >= NOTIFICATION_QUEUE_CAPACITY {
            state.closed = true;
            metrics::counter!("mux_server.notification.overflow").increment(1);
            let mut queued_kinds = BTreeMap::<&'static str, usize>::new();
            for pending in &state.queue {
                *queued_kinds.entry(notification_kind(pending)).or_default() += 1;
            }
            log::warn!(
                "closing mux client connection after notification queue reached {} items; \
                 incoming_kind={} queued_kinds={:?}",
                NOTIFICATION_QUEUE_CAPACITY,
                notification_kind(&notification),
                queued_kinds,
            );
            self.wake_tx.close();
            return false;
        }

        state.queue.push_back(notification);
        if let Some(key) = key {
            state.pending_state.insert(key);
        }
        metrics::gauge!("mux_server.notification.queue_depth").set(state.queue.len() as f64);
        let _ = self.wake_tx.try_send(());
        true
    }

    fn try_pop(&self) -> Option<MuxNotification> {
        let mut state = self.state.lock().unwrap();
        let notification = state.queue.pop_front()?;
        if let Some(key) = notification_key(&notification) {
            state.pending_state.remove(&key);
        }

        let _ = self.wake_rx.try_recv();
        if !state.queue.is_empty() {
            let _ = self.wake_tx.try_send(());
        }
        metrics::gauge!("mux_server.notification.queue_depth").set(state.queue.len() as f64);
        Some(notification)
    }

    async fn recv(&self) -> Result<MuxNotification, smol::channel::RecvError> {
        loop {
            self.wake_rx.recv().await?;
            if let Some(notification) = self.try_pop() {
                return Ok(notification);
            }
        }
    }

    fn len(&self) -> usize {
        self.state.lock().unwrap().queue.len()
    }
}

async fn write_decoded_pdu<T>(stream: &mut Async<T>, queued: QueuedPdu) -> anyhow::Result<u64>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    metrics::histogram!("mux_server.control_reply.queue_latency")
        .record(queued.queued_at.elapsed());
    let decoded = queued.decoded;
    let serial = decoded.serial;
    let start = std::time::Instant::now();
    match decoded.pdu.encode_async(stream, decoded.serial).await {
        Ok(()) => {}
        Err(err) => {
            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                if err.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(serial);
                }
            }
            return Err(err).context("encoding PDU to client");
        }
    };
    match stream.flush().await {
        Ok(()) => {
            metrics::histogram!("mux_server.control_reply.write_latency").record(start.elapsed());
            Ok(serial)
        }
        Err(err) => {
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(serial);
            }
            Err(err).context("flushing PDU to client")
        }
    }
}

async fn write_render_batch<T>(stream: &mut Async<T>, batch: RenderBatch) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    metrics::histogram!("mux_server.render_batch.queue_latency").record(batch.queued_at.elapsed());
    let start = std::time::Instant::now();
    let result = async {
        for decoded in &batch.pdus {
            decoded
                .pdu
                .encode_async(stream, decoded.serial)
                .await
                .context("encoding render PDU to client")?;
        }
        stream
            .flush()
            .await
            .context("flushing render batch to client")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    metrics::histogram!("mux_server.render_batch.write_latency").record(start.elapsed());
    metrics::histogram!("mux_server.render_batch.pdu_count").record(batch.pdus.len() as f64);
    batch.complete();
    result
}

async fn handle_notification<T>(
    stream: &mut Async<T>,
    handler: &mut SessionHandler,
    notification: MuxNotification,
) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    match notification {
        MuxNotification::PaneOutput(pane_id) => {
            handler.schedule_pane_push(pane_id);
        }
        MuxNotification::PaneAdded(_pane_id) => {}
        MuxNotification::PaneRemoved(pane_id) => {
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id })
                .encode_async(stream, 0)
                .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::Alert { pane_id, alert } => {
            {
                let per_pane = handler.per_pane(pane_id);
                let mut per_pane = per_pane.lock().unwrap();
                per_pane.push_notification(alert);
            }
            handler.schedule_pane_push(pane_id);
        }
        MuxNotification::SaveToDownloads { .. } => {}
        MuxNotification::AssignClipboard {
            pane_id,
            selection,
            clipboard,
        } => {
            Pdu::SetClipboard(codec::SetClipboard {
                pane_id,
                clipboard,
                selection,
            })
            .encode_async(stream, 0)
            .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::TabAddedToWindow { tab_id, window_id } => {
            Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id })
                .encode_async(stream, 0)
                .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::WindowRemoved(_window_id) => {}
        MuxNotification::WindowCreated(_window_id) => {}
        MuxNotification::WindowInvalidated(_window_id) => {}
        MuxNotification::WindowWorkspaceChanged(window_id) => {
            let workspace = {
                let mux = Mux::get();
                mux.get_window(window_id)
                    .map(|w| w.get_workspace().to_string())
            };
            if let Some(workspace) = workspace {
                Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                    window_id,
                    workspace,
                })
                .encode_async(stream, 0)
                .await?;
                stream.flush().await.context("flushing PDU to client")?;
            }
        }
        MuxNotification::PaneFocused(pane_id) => {
            Pdu::PaneFocused(codec::PaneFocused { pane_id })
                .encode_async(stream, 0)
                .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::TabResized { tab_id, origin } => {
            if !handler.notification_originates_here(origin.as_ref()) {
                Pdu::TabResized(codec::TabResized { tab_id })
                    .encode_async(stream, 0)
                    .await?;
                stream.flush().await.context("flushing PDU to client")?;
            }
        }
        MuxNotification::TabOrderChanged {
            window_id,
            tab_ids,
            origin,
        } => {
            if !handler.notification_originates_here(origin.as_ref()) {
                Pdu::TabOrderChanged(codec::TabOrderChanged { window_id, tab_ids })
                    .encode_async(stream, 0)
                    .await?;
                stream.flush().await.context("flushing PDU to client")?;
            }
        }
        MuxNotification::ParkedTabsChanged {
            window_id,
            tab_ids,
            parked_tab_ids,
            origin,
        } => {
            if !handler.notification_originates_here(origin.as_ref()) {
                Pdu::ParkedTabsChanged(codec::ParkedTabsChanged {
                    window_id,
                    tab_ids,
                    parked_tab_ids,
                })
                .encode_async(stream, 0)
                .await?;
                stream.flush().await.context("flushing PDU to client")?;
            }
        }
        MuxNotification::TabTitleChanged { tab_id, title: _ } => {
            let title = handler.tab_title_for_client(tab_id);
            Pdu::TabTitleChanged(title).encode_async(stream, 0).await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::WindowTitleChanged { window_id, title } => {
            Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title })
                .encode_async(stream, 0)
                .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::WorkspaceRenamed {
            old_workspace,
            new_workspace,
        } => {
            Pdu::RenameWorkspace(codec::RenameWorkspace {
                old_workspace,
                new_workspace,
            })
            .encode_async(stream, 0)
            .await?;
            stream.flush().await.context("flushing PDU to client")?;
        }
        MuxNotification::ActiveWorkspaceChanged(_) => {}
        MuxNotification::Empty => {}
    }
    Ok(())
}

pub async fn process<T>(stream: T) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream).await
}

pub async fn process_async<T>(mut stream: Async<T>) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let (reply_tx, reply_rx) = smol::channel::bounded::<QueuedPdu>(CONTROL_REPLY_QUEUE_CAPACITY);
    let (render_tx, render_rx) = smol::channel::bounded::<RenderBatch>(RENDER_BATCH_QUEUE_CAPACITY);
    let notifications = NotificationQueue::new();

    let pdu_sender = PduSender::new({
        let reply_tx = reply_tx.clone();
        move |pdu| {
            let result = reply_tx
                .try_send(pdu)
                .map_err(|e| anyhow::anyhow!("{:?}", e));
            metrics::gauge!("mux_server.control_reply.queue_depth").set(reply_tx.len() as f64);
            result
        }
    });
    let render_sender = RenderBatchSender::new({
        let render_tx = render_tx.clone();
        move |batch| {
            let result = render_tx
                .try_send(batch)
                .map_err(|e| anyhow::anyhow!("{:?}", e));
            metrics::gauge!("mux_server.render_batch.queue_depth").set(render_tx.len() as f64);
            result
        }
    });
    let mut handler = SessionHandler::new(pdu_sender, render_sender);

    let mut subscribed_to_mux = false;
    let mut inflight_control_requests = 0usize;
    let mut next_lane = 0usize;

    loop {
        let can_read = inflight_control_requests < MAX_INFLIGHT_CONTROL_REQUESTS;
        let mut ready = None;

        // Poll ready lanes in rotating order. This keeps sustained output,
        // notifications, and control traffic from starving one another.
        for offset in 0..4 {
            let lane = (next_lane + offset) % 4;
            ready = match lane {
                0 if can_read && stream.readable().now_or_never().is_some() => {
                    Some(ReadyItem::Readable)
                }
                1 => reply_rx.try_recv().ok().map(ReadyItem::WritePdu),
                2 => render_rx.try_recv().ok().map(ReadyItem::WriteRender),
                3 => notifications.try_pop().map(ReadyItem::Notif),
                _ => None,
            };
            if ready.is_some() {
                break;
            }
        }

        let item = match ready {
            Some(item) => Ok(item),
            None if can_read => {
                let reply_msg = reply_rx
                    .recv()
                    .map(|result| result.map(ReadyItem::WritePdu));
                let render_msg = render_rx
                    .recv()
                    .map(|result| result.map(ReadyItem::WriteRender));
                let notif_msg = notifications
                    .recv()
                    .map(|result| result.map(ReadyItem::Notif));
                let wait_for_read = stream.readable().map(|_| Ok(ReadyItem::Readable));
                smol::future::or(
                    wait_for_read,
                    smol::future::or(reply_msg, smol::future::or(render_msg, notif_msg)),
                )
                .await
            }
            None => {
                let reply_msg = reply_rx
                    .recv()
                    .map(|result| result.map(ReadyItem::WritePdu));
                let render_msg = render_rx
                    .recv()
                    .map(|result| result.map(ReadyItem::WriteRender));
                let notif_msg = notifications
                    .recv()
                    .map(|result| result.map(ReadyItem::Notif));
                smol::future::or(reply_msg, smol::future::or(render_msg, notif_msg)).await
            }
        };

        let item = match item {
            Ok(item) => item,
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        };
        next_lane = (item.lane() + 1) % 4;

        metrics::gauge!("mux_server.control_reply.queue_depth").set(reply_rx.len() as f64);
        metrics::gauge!("mux_server.render_batch.queue_depth").set(render_rx.len() as f64);
        metrics::gauge!("mux_server.notification.queue_depth").set(notifications.len() as f64);
        metrics::gauge!("mux_server.control_request.inflight")
            .set(inflight_control_requests as f64);

        match item {
            ReadyItem::Readable => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                if decoded.serial != 0 {
                    inflight_control_requests += 1;
                }
                handler.process_one(decoded);
                if !subscribed_to_mux && handler.wants_mux_notifications() {
                    let mux = Mux::get();
                    let notifications = Arc::clone(&notifications);
                    mux.subscribe(move |n| notifications.push(n));
                    subscribed_to_mux = true;
                }
            }
            ReadyItem::WritePdu(queued) => {
                let serial = write_decoded_pdu(&mut stream, queued).await?;
                if serial != 0 {
                    inflight_control_requests = inflight_control_requests.saturating_sub(1);
                }
            }
            ReadyItem::WriteRender(batch) => write_render_batch(&mut stream, batch).await?,
            ReadyItem::Notif(notification) => {
                handle_notification(&mut stream, &mut handler, notification).await?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{process, NotificationQueue, NOTIFICATION_QUEUE_CAPACITY};
    use codec::{Pdu, Ping, SetClientId};
    use mux::client::{ClientId, ClientViewId};
    use mux::MuxNotification;
    use std::time::Duration;
    use wakterm_uds::UnixStream;

    #[cfg(unix)]
    #[test]
    fn proxy_set_client_id_reply_is_not_blocked_waiting_for_more_input() {
        use std::os::fd::{FromRawFd, IntoRawFd};
        use std::os::unix::net::UnixStream as StdUnixStream;

        let (client_stream, server_stream) = StdUnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let server_stream = unsafe { UnixStream::from_raw_fd(server_stream.into_raw_fd()) };
        let handle = std::thread::spawn(move || smol::block_on(process(server_stream)).unwrap());

        let mut client_stream = unsafe { UnixStream::from_raw_fd(client_stream.into_raw_fd()) };
        let pdu = Pdu::SetClientId(SetClientId {
            client_id: ClientId::new(),
            view_id: ClientViewId::persistent(),
            is_proxy: true,
            client_version_string: Some(config::wakterm_version().to_owned()),
        });
        pdu.encode(&mut client_stream, 1).unwrap();

        let decoded = Pdu::decode(&mut client_stream).unwrap();
        assert!(matches!(decoded.pdu, Pdu::UnitResponse(_)));

        drop(client_stream);
        handle.join().unwrap();
    }

    #[test]
    fn notification_queue_coalesces_state_and_fails_closed_at_its_bound() {
        let queue = NotificationQueue::new();
        for _ in 0..100_000 {
            assert!(queue.push(MuxNotification::PaneOutput(42)));
        }
        assert_eq!(queue.len(), 1);
        assert!(matches!(
            queue.try_pop(),
            Some(MuxNotification::PaneOutput(42))
        ));

        assert!(queue.push(MuxNotification::WindowTitleChanged {
            window_id: 7,
            title: "old".to_string(),
        }));
        assert!(queue.push(MuxNotification::WindowTitleChanged {
            window_id: 7,
            title: "latest".to_string(),
        }));
        assert_eq!(queue.len(), 1);
        assert!(matches!(
            queue.try_pop(),
            Some(MuxNotification::WindowTitleChanged { title, .. }) if title == "latest"
        ));

        assert!(queue.push(MuxNotification::TabOrderChanged {
            window_id: 7,
            tab_ids: vec![1, 2, 3],
            origin: None,
        }));
        assert!(queue.push(MuxNotification::TabOrderChanged {
            window_id: 7,
            tab_ids: vec![3, 2, 1],
            origin: None,
        }));
        assert_eq!(queue.len(), 1);
        assert!(matches!(
            queue.try_pop(),
            Some(MuxNotification::TabOrderChanged { tab_ids, .. })
                if tab_ids == vec![3, 2, 1]
        ));

        for percent in 0..100_000 {
            assert!(queue.push(MuxNotification::Alert {
                pane_id: 42,
                alert: wakterm_term::Alert::Progress(wakterm_term::terminal::Progress::Percentage(
                    (percent % 100) as u8
                ),),
            }));
        }
        assert_eq!(queue.len(), 1);
        assert!(matches!(
            queue.try_pop(),
            Some(MuxNotification::Alert {
                pane_id: 42,
                alert: wakterm_term::Alert::Progress(wakterm_term::terminal::Progress::Percentage(
                    99
                )),
            })
        ));

        for _ in 0..NOTIFICATION_QUEUE_CAPACITY {
            assert!(queue.push(MuxNotification::Alert {
                pane_id: 42,
                alert: wakterm_term::Alert::Bell,
            }));
        }
        assert!(!queue.push(MuxNotification::Alert {
            pane_id: 42,
            alert: wakterm_term::Alert::Bell,
        }));
        assert_eq!(queue.len(), NOTIFICATION_QUEUE_CAPACITY);
    }

    #[test]
    fn ignored_mux_notifications_do_not_consume_remote_queue_capacity() {
        let queue = NotificationQueue::new();
        for _ in 0..100_000 {
            assert!(queue.push(MuxNotification::WindowInvalidated(7)));
            assert!(queue.push(MuxNotification::Empty));
        }
        assert_eq!(queue.len(), 0);

        assert!(queue.push(MuxNotification::PaneOutput(42)));
        assert_eq!(queue.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn saturated_control_requests_receive_lossless_responses() {
        use std::os::fd::{FromRawFd, IntoRawFd};
        use std::os::unix::net::UnixStream as StdUnixStream;

        let (client_stream, server_stream) = StdUnixStream::pair().unwrap();
        client_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client_stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let server_stream = unsafe { UnixStream::from_raw_fd(server_stream.into_raw_fd()) };
        let handle = std::thread::spawn(move || smol::block_on(process(server_stream)).unwrap());
        let mut client_stream = unsafe { UnixStream::from_raw_fd(client_stream.into_raw_fd()) };

        const REQUESTS: u64 = 512;
        for serial in 1..=REQUESTS {
            Pdu::Ping(Ping {})
                .encode(&mut client_stream, serial)
                .unwrap();
        }
        for expected_serial in 1..=REQUESTS {
            let decoded = Pdu::decode(&mut client_stream).unwrap();
            assert_eq!(decoded.serial, expected_serial);
            assert!(matches!(decoded.pdu, Pdu::Pong(_)));
        }

        drop(client_stream);
        handle.join().unwrap();
    }
}
