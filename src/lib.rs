mod executor;
mod room_buffer;
mod server;

use std::time::Duration;
use url::Url;

use tokio::runtime::Runtime;

use async_std;
use async_std::sync::channel as async_channel;
use async_std::sync::Receiver as AsyncReceiver;
use async_std::sync::Sender as AsyncSender;
use std::collections::HashMap;

use server::{MatrixServer, ServerMessage};

use weechat::{
    weechat_plugin, ArgsWeechat, Weechat, WeechatPlugin, WeechatResult,
};

use matrix_nio::api::r0::session::login::Response as LoginResponse;
use matrix_nio::{
    self,
    events::{
        collections::all::{RoomEvent, StateEvent},
        room::message::{MessageEventContent, TextMessageEventContent},
    },
    AsyncClient, AsyncClientConfig, SyncSettings,
};

use crate::executor::{cleanup_executor, spawn_weechat};

pub enum ThreadMessage {
    LoginMessage(LoginResponse),
    SyncState(String, StateEvent),
    SyncEvent(String, RoomEvent),
}

const PLUGIN_NAME: &str = "matrix";

struct Matrix {
    tokio: Option<Runtime>,
    servers: HashMap<String, MatrixServer>,
}

async fn sync_loop(
    mut client: AsyncClient,
    channel: AsyncSender<Result<ThreadMessage, String>>,
) {
    let sender_client = client.clone();

    let ret = client.login("example", "wordpass", None).await;

    match ret {
        Ok(response) => {
            channel
                .send(Ok(ThreadMessage::LoginMessage(response)))
                .await
        }
        Err(e) => {
            channel.send(Err("No logging in".to_string())).await;
            return;
        }
    }
    let mut sync_token = None;

    loop {
        let sync_settings = SyncSettings::new().timeout(30000).unwrap();
        let sync_settings = if let Some(ref token) = sync_token {
            sync_settings.token(token)
        } else {
            sync_settings
        };

        let response = client.sync(sync_settings).await;

        match response {
            Ok(r) => {
                sync_token = Some(r.next_batch);

                for (room_id, room) in r.rooms.join {
                    for event in room.state.events {
                        let event = match event.into_result() {
                            Ok(e) => e,
                            Err(e) => continue,
                        };
                        channel
                            .send(Ok(ThreadMessage::SyncState(
                                room_id.to_string(),
                                event,
                            )))
                            .await;
                    }

                    for event in room.timeline.events {
                        let event = match event.into_result() {
                            Ok(e) => e,
                            Err(e) => continue,
                        };
                        channel
                            .send(Ok(ThreadMessage::SyncEvent(
                                room_id.to_string(),
                                event,
                            )))
                            .await;
                    }
                }
            }
            Err(e) => {
                let err = format!("{:?}", e.to_string());
                channel.send(Err(err)).await;
                async_std::task::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn send_loop(
    mut client: AsyncClient,
    channel: AsyncReceiver<ServerMessage>,
) {
    while let Some(message) = channel.recv().await {
        match message {
            ServerMessage::ShutDown => return,
            ServerMessage::RoomSend(room_id, message) => {
                let content =
                    MessageEventContent::Text(TextMessageEventContent {
                        body: message.to_owned(),
                        format: None,
                        formatted_body: None,
                        relates_to: None,
                    });

                let ret = client.room_send(&room_id, content).await;

                match ret {
                    Ok(r) => (),
                    Err(e) => (),
                }
            }
        }
    }
}

impl WeechatPlugin for Matrix {
    fn init(weechat: &Weechat, _args: ArgsWeechat) -> WeechatResult<Self> {
        let runtime = Runtime::new().unwrap();
        let (tx, rx) = async_channel(1000);

        let weechat_task = async move {
            let weechat = unsafe { Weechat::weechat() };
            let plugin = plugin();

            let mut server = plugin.servers.get_mut("localhost").unwrap();

            loop {
                let ret = match rx.recv().await {
                    Some(m) => m,
                    None => {
                        weechat.print("Error receiving message");
                        return;
                    }
                };

                match ret {
                    Ok(message) => match message {
                        ThreadMessage::LoginMessage(r) => {
                            server.receive_login(r)
                        }
                        ThreadMessage::SyncEvent(r, e) => {
                            server.receive_joined_timeline_event(&r, e)
                        }
                        ThreadMessage::SyncState(r, e) => {
                            server.receive_joined_state_event(&r, e)
                        }
                        _ => (),
                    },
                    Err(e) => weechat.print(&format!("Ruma error {}", e)),
                };
            }
        };
        let homeserver = Url::parse("http://localhost:8008").unwrap();

        let config = AsyncClientConfig::new()
            .proxy("http://localhost:8080")
            .unwrap()
            .disable_ssl_verification();
        let client =
            AsyncClient::new_with_config(homeserver.clone(), None, config)
                .unwrap();
        let send_client = client.clone();

        runtime.spawn(async move {
            sync_loop(client, tx).await;
        });

        let (tx, rx) = async_channel(10);

        let server_name = "localhost";

        let server = MatrixServer::new(server_name, &homeserver, tx);
        let mut servers = HashMap::new();
        servers.insert(server_name.to_owned(), server);

        runtime.spawn(async move {
            send_loop(send_client, rx).await;
        });

        spawn_weechat(weechat_task);

        Ok(Matrix {
            tokio: Some(runtime),
            servers,
        })
    }
}

impl Drop for Matrix {
    fn drop(&mut self) {
        let runtime = self.tokio.take();

        if let Some(r) = runtime {
            r.shutdown_now();
        }
        cleanup_executor();
    }
}

weechat_plugin!(
    Matrix,
    name: "matrix",
    author: "Damir Jelić <poljar@termina.org.uk>",
    description: "Matrix protocol",
    version: "0.1.0",
    license: "ISC"
);
