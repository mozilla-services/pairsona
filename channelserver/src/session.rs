use std::time::{Duration, Instant};

use actix::{
    fut, Actor, ActorContext, ActorFuture, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws;
use cadence::{Counted, StatsdClient};
use ipnet::IpNet;
use maxminddb;
use slog::{debug, error, info, warn};

use crate::channelid::ChannelID;
use crate::logging;
use crate::meta::SenderData;
use crate::server;

/// This is our websocket route state, this state is shared with all route
/// instances via `HttpContext::state()`
pub struct WsChannelSessionState {
    pub addr: Addr<server::ChannelServer>,
    pub log: logging::MozLogger,
    pub iploc: maxminddb::Reader<Vec<u8>>,
    pub metrics: StatsdClient,
    pub trusted_proxy_list: Vec<IpNet>,
    pub connection_lifespan: u64,
    pub client_timeout: u64,
    pub ping_interval: u64,
}

pub struct WsChannelSession {
    /// unique session id
    pub id: server::SessionId,
    /// Client must send ping at least once per 10 seconds, otherwise we drop
    /// connection.
    pub hb: Instant,
    /// Max channel lifespan
    pub expiry: Instant,
    /// joined channel
    pub channel: ChannelID,
    /// peer name
    pub meta: SenderData,
    /// is this the first request for the given channel?
    pub initial_connect: bool,
}

impl Actor for WsChannelSession {
    type Context = ws::WebsocketContext<Self, WsChannelSessionState>;

    /// Method is called on actor start.
    /// We register ws session with ChannelServer
    fn started(&mut self, ctx: &mut Self::Context) {
        // register self in chat server. `AsyncContext::wait` register
        // future within context, but context waits until this future resolves
        // before processing any other events.
        // HttpContext::state() is instance of WsChatSessionState, state is shared
        // across all routes within application

        self.hb(ctx);

        let meta = self.meta.clone();
        let addr: Addr<Self> = ctx.address();
        ctx.state()
            .addr
            .send(server::Connect {
                addr: addr.recipient(),
                channel: self.channel,
                remote: self.meta.remote.clone(),
                initial_connect: self.initial_connect,
            })
            .into_actor(self)
            .then(|res, act, ctx| {
                match res {
                    Ok(session_id) => {
                        if session_id == 0 {
                            ctx.stop();
                            return fut::err(());
                        }
                        debug!(
                            "Starting new session";
                            "session" => session_id,
                            "remote_ip" => meta.remote,
                        );
                        ctx.state().metrics.incr("conn.create").ok();
                        act.id = session_id;
                    }
                    // something is wrong with chat server
                    Err(err) => {
                        error!(
                            ctx.state().log.log,
                            "Unhandled Error: {:?}", err;
                            "remote_ip"=> meta.remote,
                        );
                        ctx.stop()
                    }
                }
                fut::ok(())
            })
            .wait(ctx);
    }

    /// Stop Session and alert all others in channel to shut down.
    fn stopping(&mut self, ctx: &mut Self::Context) -> Running {
        // notify chat server
        debug!(
            ctx.state().log.log,
            "Killing session";
            "session" => &self.id,
            "remote_ip" => &self.meta.remote,
        );
        ctx.state().addr.do_send(server::Disconnect {
            channel: self.channel,
            id: self.id,
            reason: server::DisconnectReason::None,
        });
        Running::Stop
    }
}

/// Handle messages from chat server, we simply send it to peer websocket
impl Handler<server::TextMessage> for WsChannelSession {
    type Result = ();

    fn handle(&mut self, msg: server::TextMessage, ctx: &mut Self::Context) {
        match msg.0 {
            server::MessageType::Terminate => {
                debug!(
                    ctx.state().log.log,
                    "Closing session";
                    "session"=> &self.id,
                    "remote_ip" => &self.meta.remote
                );

                ctx.close(Some(ws::CloseCode::Normal.into()));
            }
            server::MessageType::Text => ctx.text(msg.1),
        }
    }
}

/// WebSocket message handler
impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChannelSession {
    fn handle(&mut self, msg: ws::Message, ctx: &mut Self::Context) {
        debug!(ctx.state().log.log,
        "Websocket Message: {:?}", msg;
        "remote_ip" => &self.meta.remote
        );
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                self.hb = Instant::now();
                ctx.pong(&msg);
            }
            Ok(ws::Message::Pong(msg)) => {
                self.hb = Instant::now();
            }
            Ok(ws::Message::Text(text)) => {
                self.hb = Instant::now();
                let mut m = text.trim();
                ctx.state().addr.do_send(server::ClientMessage {
                    id: self.id,
                    message_type: server::MessageType::Text,
                    message: m.to_owned(),
                    channel: self.channel,
                    sender: self.meta.clone(),
                })
            }
            Ok(ws::Message::Binary(bin)) => {
                info!(
                    ctx.state().log.log,
                    "TODO: Binary format not supported";
                    "remote_ip"=> &self.meta.remote,
                );
            }
            Ok(ws::Message::Close(_)) => {
                ctx.state().addr.do_send(server::Disconnect {
                    id: self.id,
                    channel: self.channel,
                    reason: server::DisconnectReason::None,
                });
                debug!(
                    ctx.state().log.log,
                    "Shutting down session";
                    "session" => &self.id,
                    "remote_ip" => &self.meta.remote,
                );
                ctx.stop();
            }
        }
    }
}
