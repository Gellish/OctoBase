use super::*;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path,
    },
    response::Response,
};
use dashmap::mapref::{entry::Entry, one::RefMut};
use futures::{sink::SinkExt, stream::StreamExt};
use jwst::{encode_update, Workspace};
use jwst_logger::error;
use jwst_logger::info;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Serialize)]
pub struct WebSocketAuthentication {
    protocol: String,
}

pub fn make_ws_route() -> Router {
    Router::new().route("/:id", get(ws_handler))
}

#[derive(Deserialize)]
struct Param {
    token: String,
}

async fn ws_handler(
    Extension(ctx): Extension<Arc<Context>>,
    Path(workspace): Path<String>,
    Query(Param { token }): Query<Param>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.protocols(["AFFiNE"])
        .on_upgrade(|socket| async move { handle_socket(socket, workspace, ctx.clone()).await })
}

fn subscribe_handler(
    context: Arc<Context>,
    workspace: &mut Workspace,
    uuid: String,
    ws_id: String,
) {
    let sub = workspace.observe(move |_, e| {
        let update = encode_update(&e.update);

        let context = context.clone();
        let uuid = uuid.clone();
        let ws_id = ws_id.clone();
        tokio::spawn(async move {
            let mut closed = vec![];

            for item in context.channel.iter() {
                let ((ws, id), tx) = item.pair();
                if &ws_id == ws && id != &uuid {
                    if tx.is_closed() {
                        closed.push(id.clone());
                    } else if let Err(e) = tx.send(Message::Binary(update.clone())).await {
                        if !tx.is_closed() {
                            error!("on observe_update error: {}", e);
                        }
                    }
                }
            }
            for id in closed {
                context.channel.remove(&(ws_id.clone(), id));
            }
        });
    });
    std::mem::forget(sub);
}

async fn handle_socket(socket: WebSocket, workspace: String, context: Arc<Context>) {
    let (mut socket_tx, mut socket_rx) = socket.split();
    let (tx, mut rx) = channel(100);

    {
        // socket thread
        let workspace = workspace.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Err(e) = socket_tx.send(msg).await {
                    error!("send error: {}", e);
                    break;
                }
            }
            info!("socket final: {}", workspace);
        });
    }

    {
        let workspace_id = workspace.clone();
        let context = context.clone();
        tokio::spawn(async move {
            use tokio::time::{sleep, Duration};
            loop {
                sleep(Duration::from_secs(10)).await;

                if let Some(workspace) = context.workspace.get(&workspace_id) {
                    let update = workspace.lock().await.sync_migration();
                    if let Err(e) = context.docs.full_migrate(&workspace_id, update).await {
                        error!("db write error: {}", e.to_string());
                    }
                } else {
                    break;
                }
            }
        });
    }

    let uuid = Uuid::new_v4().to_string();
    context
        .channel
        .insert((workspace.clone(), uuid.clone()), tx.clone());

    if let Ok(init_data) = {
        let ws = match init_workspace(&context, &workspace).await {
            Ok(doc) => doc,
            Err(e) => {
                error!("Failed to init doc: {}", e);
                return;
            }
        };

        let mut ws = ws.lock().await;

        subscribe_handler(context.clone(), &mut ws, uuid.clone(), workspace.clone());

        ws.sync_init_message()
    } {
        if tx.send(Message::Binary(init_data)).await.is_err() {
            context.channel.remove(&(workspace, uuid));
            // client disconnected
            return;
        }
    } else {
        context.channel.remove(&(workspace, uuid));
        // client disconnected
        return;
    }

    while let Some(msg) = socket_rx.next().await {
        if let Ok(Message::Binary(binary)) = msg {
            let payload = {
                let workspace = context.workspace.get(&workspace).unwrap();
                let mut workspace = workspace.value().lock().await;

                use std::panic::{catch_unwind, AssertUnwindSafe};
                catch_unwind(AssertUnwindSafe(|| workspace.sync_decode_message(&binary)))
            };
            if let Ok(messages) = payload {
                for reply in messages {
                    if let Err(e) = tx.send(Message::Binary(reply)).await {
                        if !tx.is_closed() {
                            error!("socket send error: {}", e.to_string());
                        }
                        // client disconnected
                        return;
                    }
                }
            }
        }
    }

    context.channel.remove(&(workspace, uuid));
}

pub async fn init_workspace<'a>(
    context: &'a Context,
    workspace: &str,
) -> Result<RefMut<'a, String, Mutex<Workspace>>, anyhow::Error> {
    match context.workspace.entry(workspace.to_owned()) {
        Entry::Vacant(entry) => {
            let doc = context.docs.create_doc(workspace).await?;

            Ok(entry.insert(Mutex::new(Workspace::from_doc(doc, workspace))))
        }
        Entry::Occupied(o) => Ok(o.into_ref()),
    }
}
