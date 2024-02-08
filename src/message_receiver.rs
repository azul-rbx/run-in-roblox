#![allow(clippy::unused_async)]
#![allow(dead_code)]

use anyhow::Result;
use async_channel::RecvError;
use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    routing::{get, post},
    Router,
};

use dashmap::DashMap;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle, time::sleep};

#[derive(Debug, Clone)]
pub enum Message {
    Start { server: String },
    Stop { server: String },
    Messages(Vec<RobloxMessage>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum RobloxMessage {
    Output {
        level: OutputLevel,
        body: String,
        server: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartMessage {
    server: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StopMessage {
    server: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusMessage {
    run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum OutputLevel {
    Print,
    Info,
    Warning,
    Error,
    ScriptError,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum RobloxEvent {
    RunScript { script: String, oneshot: bool },
    Deregister { no_exit: bool },
}

#[derive(Debug, Clone)]
pub struct Svc {
    message_tx: async_channel::Sender<Message>,
    message_rx: async_channel::Receiver<Message>,
    shutdown_tx: async_channel::Sender<()>,
    shutdown_rx: async_channel::Receiver<()>,
    instances: Arc<DashMap<String, Arc<RwLock<StudioInstance>>>>,
}

#[derive(Debug)]
pub struct StudioInstance {
    service: Arc<Svc>,
    id: String,
    event_queue: Vec<RobloxEvent>,
    stale_remover: Option<JoinHandle<()>>,
}

impl StudioInstance {
    fn new(id: String, service: Arc<Svc>) -> Self {
        Self {
            id,
            service,
            event_queue: Vec::new(),
            stale_remover: None,
        }
    }

    fn create_stale_task(&mut self) {
        let service_clone = self.service.clone();
        let id_clone = self.id.clone();
        self.stale_remover = Some(tokio::task::spawn(async move {
            sleep(Duration::from_secs(10)).await;
            let service = service_clone;
            info!("removing studio server {} as it is now stale", &id_clone);
            service.instances.remove(&id_clone);
            service
                .message_tx
                .send(Message::Stop { server: id_clone })
                .await
                .unwrap();
        }));
    }

    fn freshen(&mut self) {
        if let Some(stale_task) = &self.stale_remover {
            stale_task.abort();
        }
        self.create_stale_task();
    }
}

impl Svc {
    async fn root(State(_): State<Arc<Self>>) -> &'static str {
        "OK!"
    }

    async fn ping(
        State(svc): State<Arc<Self>>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Result<&'static str, StatusCode> {
        let server_id = params.get("server").ok_or(StatusCode::BAD_REQUEST)?;
        if let Some(server) = svc.instances.get(server_id) {
            debug!("studio server {server_id:} checked in");
            let mut server = server.write().await;
            server.freshen();
            drop(server);
            Ok("OK!")
        } else {
            Err(StatusCode::UNPROCESSABLE_ENTITY)
        }
    }

    async fn start_handler(
        State(svc): State<Arc<Self>>,
        Json(start_message): Json<StartMessage>,
    ) -> Result<&'static str, StatusCode> {
        svc.create_server(start_message.server.clone()).await;
        svc.message_tx
            .send(Message::Start {
                server: start_message.server.clone(),
            })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok("Started")
    }

    async fn stop_handler(
        State(svc): State<Arc<Self>>,
        Json(stop_message): Json<StopMessage>,
    ) -> Result<&'static str, StatusCode> {
        svc.message_tx
            .send(Message::Stop {
                server: stop_message.server.clone(),
            })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok("Stopped")
    }

    async fn events_handler(
        State(svc): State<Arc<Self>>,
        Query(params): Query<HashMap<String, String>>,
    ) -> Result<Json<Vec<RobloxEvent>>, StatusCode> {
        let server = params.get("server").ok_or(StatusCode::BAD_REQUEST)?;
        if let Some(server) = svc.instances.get(server) {
            let mut server = server.write().await;
            let events_clone = server.event_queue.clone();
            server.event_queue.clear();
            drop(server);
            Ok(Json(events_clone))
        } else {
            Err(StatusCode::NOT_FOUND)
        }
    }

    async fn messages_handler(
        State(svc): State<Arc<Self>>,
        Json(messages): Json<Vec<RobloxMessage>>,
    ) -> Result<&'static str, StatusCode> {
        svc.message_tx
            .send(Message::Messages(messages.clone()))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        Ok("Got it!")
    }

    async fn status_handler(
        State(_svc): State<Arc<Self>>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> Result<Json<StatusMessage>, StatusCode> {
        Ok(Json(StatusMessage { run: true }))
    }

    pub async fn start() -> Result<Arc<Self>> {
        let (message_tx, message_rx) = async_channel::bounded(100);
        let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);

        let svc = Arc::new(Self {
            message_tx,
            message_rx,
            shutdown_tx,
            shutdown_rx,
            instances: Arc::new(DashMap::new()),
        });

        let svc_clone = svc.clone();
        let shutdown_signal = async move { svc_clone.shutdown_rx.recv().await.unwrap() };

        let svc_clone = svc.clone();
        let app: Router = Router::new()
            .route("/", get(Self::root))
            .route("/ping", get(Self::ping))
            .route("/start", post(Self::start_handler))
            .route("/stop", post(Self::stop_handler))
            .route("/messages", post(Self::messages_handler))
            .route("/events", get(Self::events_handler))
            .route("/status", get(Self::status_handler))
            .with_state(svc_clone);

        let listener = TcpListener::bind("127.0.0.1:7777").await?;
        tokio::task::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal)
                .await
                .unwrap();
        });
        Ok(svc)
    }

    pub async fn queue_event(&self, server: String, msg: RobloxEvent) {
        debug!("queuing message {msg:?} for server {server:}");
        if let Some(server) = self.instances.get(&server) {
            let mut server = server.write().await;
            server.event_queue.push(msg);
        } else {
            info!("could not find server instance for {server:}, creating new one..");
            let server = self.create_server(server).await;
            let mut server = server.write().await;
            server.event_queue.push(msg);
        };
    }

    pub async fn create_server(&self, server: String) -> Arc<RwLock<StudioInstance>> {
        debug!("creating new server {server:}");
        let mut instance = StudioInstance::new(server.clone(), Arc::new(self.clone()));
        instance.create_stale_task();
        let instance = Arc::new(RwLock::new(instance));
        self.instances.insert(server, instance.clone());
        instance
    }

    pub async fn recv(&self) -> Message {
        self.message_rx.recv().await.unwrap()
    }

    pub async fn recv_timeout(&self, timeout: Duration) -> Option<Result<Message, RecvError>> {
        (tokio::time::timeout(timeout, self.message_rx.recv()).await).ok()
    }

    pub async fn stop(&self) {
        self.shutdown_tx.send(()).await.unwrap();
    }
}
