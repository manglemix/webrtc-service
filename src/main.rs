use std::{
    collections::hash_map::Entry,
    mem::MaybeUninit,
    sync::atomic::{AtomicU16, Ordering},
};

use fxhash::FxHashMap;
use hypermangle_core::auto_main;
use axum::{
    extract::{ws::Message, WebSocketUpgrade},
    response::Response,
    routing::get, Router,
};
use rand::{thread_rng, Rng};
use serde_json::{json, to_string, Value};
use tokio::sync::{mpsc, RwLock};

struct Room {
    new_offers_sender: mpsc::Sender<ConnectingClient>,
    client_ids_counter: AtomicU16,
    ice_to_host: mpsc::Sender<(u16, String)>,
    finished_clients_sender: mpsc::Sender<u16>
}

struct ConnectingClient {
    client_id: u16,
    offer: String,
    response_sender: mpsc::Sender<String>,
}

const MAX_ROOM_CODE: u32 = 1_000_000;
static mut ROOMS: MaybeUninit<RwLock<FxHashMap<u32, Room>>> = MaybeUninit::uninit();

#[inline]
fn get_rooms() -> &'static RwLock<FxHashMap<u32, Room>> {
    unsafe { ROOMS.assume_init_ref() }
}

#[axum::debug_handler]
async fn host_webrtc(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut ws| async move {
        macro_rules! send {
            ($msg: expr) => {
                let Ok(_) = ws.send(Message::Text($msg)).await else { break };
            };
            (json $msg: expr) => {
                send!(to_string(&$msg).unwrap())
            }
        }

        let (new_offers_sender, mut new_offers_recv) = mpsc::channel(3);
        let (ice_to_host, mut ice_to_host_recv) = mpsc::channel(3);
        let (finished_clients_sender, mut finished_clients_recv) = mpsc::channel(1);

        let room = Room {
            new_offers_sender,
            client_ids_counter: AtomicU16::new(3),
            ice_to_host,
            finished_clients_sender
        };

        let code = {
            let mut rooms = get_rooms().write().await;
            let mut rng = thread_rng();

            loop {
                let code = rng.gen_range(0..MAX_ROOM_CODE);
                match rooms.entry(code) {
                    Entry::Occupied(_) => { }
                    Entry::Vacant(entry) => {
                        entry.insert(room);
                        break code
                    }
                }
            }
        };

        let Ok(_) = ws.send(Message::Text(code.to_string())).await else { return };
        let mut response_senders: FxHashMap<u16, mpsc::Sender<String>> = FxHashMap::default();

        loop {
            tokio::select! {
                option = new_offers_recv.recv() => {
                    let Some(ConnectingClient { client_id, offer, response_sender }) = option else {
                        break
                    };
                    send!(json json!({"new_offer": offer, "id": client_id}));
                    response_senders.insert(client_id, response_sender);
                }
                option = ws.recv() => {
                    let Some(Ok(Message::Text(msg))) = option else {
                        break
                    };
                    
                    let Ok(mut msg) = serde_json::from_str::<FxHashMap<String, Value>>(&msg) else { break };
                    let Some(client_id) = msg.get("id").map(Value::as_i64).flatten() else { break };
                    let Ok(client_id) = client_id.try_into() else { break };
                    let Some(sender) = response_senders.get(&client_id) else { break };

                    if let Some(Value::String(response)) = msg.remove("answer") {
                        if sender.send(response).await.is_err() {
                            response_senders.remove(&client_id);
                        };
                    } else if let Some(ice) = msg.remove("ice") {
                        if sender.send(to_string(&ice).unwrap()).await.is_err() {
                            response_senders.remove(&client_id);
                        };
                    } else {
                        break
                    }
                }
                option = ice_to_host_recv.recv() => {
                    let Some((client_id, ice)) = option else {
                        break
                    };
                    send!(json json!({"ice": ice, "id": client_id}));
                }
                option = finished_clients_recv.recv() => {
                    let Some(client_id) = option else { break };
                    response_senders.remove(&client_id);
                }
            }
        }

        get_rooms().write().await.remove(&code);
    })
}

#[axum::debug_handler]
async fn join_webrtc(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut ws| async move {
        macro_rules! send {
            ($msg: expr) => {
                let Ok(_) = ws.send(Message::Text($msg)).await else {
                    return;
                };
            };
            (json $msg: expr) => {
                send!(to_string(&$msg).unwrap())
            };
        }

        macro_rules! recv {
            () => {{
                let Some(Ok(Message::Text(msg))) = ws.recv().await else {
                    return;
                };
                msg
            }};
        }

        let client_id;
        let Ok(code) = recv!().parse::<u32>() else {
            return;
        };
        {
            let rooms = get_rooms().read().await;
            let Some(room) = rooms.get(&code) else { return };
            client_id = room.client_ids_counter.fetch_add(1, Ordering::Relaxed);
        }
        send!(client_id.to_string());

        let offer = recv!();
        let (response_sender, mut response_receiver) = mpsc::channel(2);
        {
            let rooms = get_rooms().read().await;
            let Some(room) = rooms.get(&code) else { return };
            let Ok(_) = room
                .new_offers_sender
                .send(ConnectingClient {
                    client_id,
                    offer,
                    response_sender,
                })
                .await
            else {
                return;
            };
        }

        let Some(answer) = response_receiver.recv().await else {
            return;
        };
        send!(answer);

        loop {
            tokio::select! {
                option = response_receiver.recv() => {
                    let Some(ice) = option else { break };
                    send!(ice);
                }
                option = ws.recv() => {
                    let rooms = get_rooms().read().await;
                    let Some(room) = rooms.get(&code) else { break };

                    let Some(Ok(Message::Text(ice))) = option else {
                        let _ = room.finished_clients_sender.send(client_id);
                        break
                    };

                    let Ok(_) = room.ice_to_host.send((client_id, ice)).await else { break };
                }
            }
        }
    })
}

fn main() {
    unsafe {
        ROOMS.write(RwLock::default());
    }

    auto_main(
        Router::new()
            .route("/host", get(host_webrtc))
            .route("/join", get(join_webrtc))
    );
}
