use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{DecodedPdu, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use smol::prelude::*;
use smol::Async;
use wakterm_uds::UnixStream;

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

enum ReadyItem {
    Notif(MuxNotification),
    WritePdu(DecodedPdu),
    Readable,
}

async fn write_decoded_pdu<T>(stream: &mut Async<T>, decoded: DecodedPdu) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    match decoded.pdu.encode_async(stream, decoded.serial).await {
        Ok(()) => {}
        Err(err) => {
            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                if err.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
            }
            return Err(err).context("encoding PDU to client");
        }
    };
    match stream.flush().await {
        Ok(()) => Ok(()),
        Err(err) => {
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            Err(err).context("flushing PDU to client")
        }
    }
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
                per_pane.notifications.push(alert);
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
        MuxNotification::TabResized(tab_id) => {
            let dominated_by_self = handler.recent_resize_tab(tab_id);
            if !dominated_by_self {
                Pdu::TabResized(codec::TabResized { tab_id })
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

    let (reply_tx, reply_rx) = smol::channel::unbounded::<DecodedPdu>();
    let (notif_tx, notif_rx) = smol::channel::unbounded::<MuxNotification>();

    let pdu_sender = PduSender::new({
        let reply_tx = reply_tx.clone();
        move |pdu| {
            reply_tx
                .try_send(pdu)
                .map_err(|e| anyhow::anyhow!("{:?}", e))
        }
    });
    let mut handler = SessionHandler::new(pdu_sender);

    let mut subscribed_to_mux = false;

    loop {
        if let Ok(decoded) = reply_rx.try_recv() {
            write_decoded_pdu(&mut stream, decoded).await?;
            continue;
        }
        if let Ok(notification) = notif_rx.try_recv() {
            handle_notification(&mut stream, &mut handler, notification).await?;
            continue;
        }

        let reply_msg = reply_rx
            .recv()
            .map(|result| result.map(ReadyItem::WritePdu));
        let notif_msg = notif_rx.recv().map(|result| result.map(ReadyItem::Notif));
        let wait_for_read = stream.readable().map(|_| Ok(ReadyItem::Readable));

        match smol::future::or(reply_msg, smol::future::or(wait_for_read, notif_msg)).await {
            Ok(ReadyItem::Readable) => {
                let decoded = match Pdu::decode_async(&mut stream, None).await {
                    Ok(data) => data,
                    Err(err) => {
                        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                        }
                        return Err(err).context("reading Pdu from client");
                    }
                };
                handler.process_one(decoded);
                if !subscribed_to_mux && handler.wants_mux_notifications() {
                    let mux = Mux::get();
                    let tx = notif_tx.clone();
                    mux.subscribe(move |n| tx.try_send(n).is_ok());
                    subscribed_to_mux = true;
                }
            }
            Ok(ReadyItem::WritePdu(decoded)) => write_decoded_pdu(&mut stream, decoded).await?,
            Ok(ReadyItem::Notif(notification)) => {
                handle_notification(&mut stream, &mut handler, notification).await?
            }
            Err(err) => {
                log::error!("process_async Err {}", err);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::process;
    use codec::{Pdu, SetClientId};
    use mux::client::{ClientId, ClientViewId};
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
}
