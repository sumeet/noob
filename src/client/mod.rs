use tokio_core;
use hyper;
use hyper_tls;
use native_tls;
use futures;
use serde_json;
use websocket;
use std;

use futures::prelude::*;
use std::str::FromStr;

pub mod events;
pub mod objects;

pub enum Error {
    HTTPError(hyper::Error),
    TLSError(native_tls::Error),
    WebsocketError(websocket::WebSocketError),
    JSONError(serde_json::Error),
    AuthenticationFailed,
    UnexpectedResponse(String),
    NotReady,
    UhWhat(String)
}

impl std::fmt::Debug for Error {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.write_str(std::error::Error::description(self))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.write_str(std::error::Error::description(self))
    }
}

impl std::error::Error for Error {
    fn description(&self) -> &str {
        match *self {
            Error::HTTPError(ref err) => std::error::Error::description(err),
            Error::TLSError(ref err) => std::error::Error::description(err),
            Error::WebsocketError(ref err) => std::error::Error::description(err),
            Error::JSONError(ref err) => std::error::Error::description(err),
            Error::AuthenticationFailed => "Authentication Failed",
            Error::UnexpectedResponse(ref msg) => &msg,
            Error::UhWhat(ref msg) => &msg,
            Error::NotReady => "The client is in the wrong state to do that."
        }
    }
}

type WebSocket = websocket::client::async::Client<Box<websocket::stream::async::Stream + Send>>;

enum ConnectionState {
    Disconnected,
    Connecting,
    Connected(WebSocket),
    Ready(WebSocket),
    Failed(Error),
}

#[derive(Debug, Clone)]
struct BotAuthorizationScheme {
    token: String,
}

impl FromStr for BotAuthorizationScheme {
    type Err = Error;
    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Ok(BotAuthorizationScheme {
            token: token.to_owned(),
        })
    }
}

impl hyper::header::Scheme for BotAuthorizationScheme {
    fn scheme() -> Option<&'static str> {
        Some("Bot")
    }
    fn fmt_scheme(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(fmt, "{}", self.token)
    }
}

pub struct Client {
    handle: tokio_core::reactor::Handle,
    gateway_url: String,
    connection: ConnectionState,
    handler: PacketHandler,
    token: String,
    http_client: hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>

}

impl Client {
    pub fn login_bot(
        handle: &tokio_core::reactor::Handle,
        token: &str,
        event_callback: Box<Fn(events::Event, &Interface) -> ()>
    ) -> Box<Future<Item = Client, Error = Error>> {
        let token = token.to_owned();
        let handle = handle.clone();
        let http = hyper::Client::configure()
            .connector(fut_try!(
                hyper_tls::HttpsConnector::new(1, &handle).map_err(|e| Error::TLSError(e))
            ))
            .build(&handle);
        let mut request = hyper::Request::new(
            hyper::Method::Get,
            fut_try!(
                hyper::Uri::from_str("https://discordapp.com/api/v6/gateway/bot")
                    .map_err(|e| Error::HTTPError(e.into()))
            ),
        );
        let auth_header = hyper::header::Authorization(
            fut_try!(BotAuthorizationScheme::from_str(&token)),
        );
        request.headers_mut().set(auth_header.clone());
        Box::new(
            http.request(request)
                .map_err(|e| Error::HTTPError(e))
                .and_then(|response| {
                    println!("{:?}", response);
                    let status = response.status();
                    if status == hyper::StatusCode::Unauthorized {
                        return Err(Error::AuthenticationFailed);
                    }
                    if status == hyper::StatusCode::Ok {
                        return Ok(response.body());
                    }
                    Err(Error::UnexpectedResponse(format!(
                        "Gateway request responded with unexpected status {}",
                        status
                    )))
                })
                .and_then(|body| body.concat2().map_err(|e| Error::HTTPError(e)))
                .and_then(
                    |chunk| -> Box<futures::future::Future<Item = Client, Error = Error>> {
                        let value: serde_json::Value = match serde_json::from_slice(&chunk) {
                            Ok(value) => value,
                            Err(err) => {
                                return Box::new(futures::future::err(Error::UnexpectedResponse(
                                    format!("Unable to parse gateway API response: {:?}", err),
                                )))
                            }
                        };
                        println!("{}", value);
                        let mut client = Client::new(handle, match value["url"].as_str() {
                            None => return Box::new(futures::future::err(Error::UnexpectedResponse(
                                        "Gateway URI was not a string".to_owned()
                                        ))),
                            Some(x) => x.to_owned()
                        }, token, event_callback, http, auth_header);
                        Box::new(client.connect())
                    },
                ),
        )
    }
    fn new(
        handle: tokio_core::reactor::Handle,
        gateway_url: String,
        token: String,
        event_callback: Box<Fn(events::Event, &Interface) -> ()>,
        http_client: hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>,
        auth_header: hyper::header::Authorization<BotAuthorizationScheme>
        ) -> Self {
        Client {
            handle: handle.clone(),
            gateway_url,
            connection: ConnectionState::Disconnected,
            token,
            handler: PacketHandler::new(event_callback, handle, auth_header),
            http_client
        }
    }
    fn connect(mut self) -> Box<futures::future::Future<Item = Client, Error = Error>> {
        if match self.connection {
            ConnectionState::Disconnected => true,
            ConnectionState::Connecting => false,
            ConnectionState::Connected(_) => false,
            ConnectionState::Ready(_) => return Box::new(futures::future::ok(self)),
            ConnectionState::Failed(_) => true,
        } {
            self.connection = ConnectionState::Connecting;
            let uri = format!("{}/?v=6&encoding=json", self.gateway_url);
            let builder = match websocket::ClientBuilder::new(
                &uri,
            ) {
                Ok(builder) => builder,
                Err(err) => {
                    return Box::new(futures::future::err(Error::UnexpectedResponse(
                        format!("Unable to parse gateway URI {}: {}", uri, err),
                    )))
                }
            };
            let connection: websocket::client::async::ClientNew<Box<websocket::async::Stream + Send>> = builder.async_connect(None, &self.handle);
            return Box::new(connection
                .and_then(move |(socket, _)| {
                    self.connection = ConnectionState::Connected(socket);
                    println!("hi");
                    futures::future::ok(self)
                })
                .map_err(|e|Error::WebsocketError(e)));
        }
        Box::new(futures::future::ok(self))
    }
    pub fn run(mut self) -> Box<futures::future::Future<Item=(),Error=Error>> {
        let mut handler = self.handler;
        let token = self.token;
        let http_client = self.http_client;
        match self.connection {
            ConnectionState::Connected(socket) => {
                let (send, recv) = futures::sync::mpsc::channel::<websocket::OwnedMessage>(64);
                let (sink, stream) = socket.split();
                let input = stream.map_err(|e|Error::WebsocketError(e))
                    .for_each(move |packet| {
                        if let Err(err) = handler.handle_message(packet, &token, &send, &http_client) {
                            eprintln!("Error handling message: {}", err);
                        }
                        Ok(())
                    });
                let output = sink.sink_map_err(|e|Error::WebsocketError(e)).send_all(recv.map_err(|e|Error::UhWhat(format!("this shouldn't be happening"))));
                Box::new(input.join(output).map(|_|()))
            },
            _ => Box::new(futures::future::err(Error::NotReady)),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Packet {
    op: u8,
    d: serde_json::Value,
    s: Option<u64>,
    t: Option<String>
}

#[must_use]
#[derive(Serialize)]
pub struct MessageBuilder<'a> {
    #[serde(skip)]
    interface: &'a Interface<'a>,
    #[serde(skip)]
    channel: objects::Snowflake,
    content: String,
    tts: bool,
    nonce: Option<objects::Snowflake>
}

impl<'a> MessageBuilder<'a> {
    fn new<'b>(interface: &'b Interface, channel: objects::Snowflake, content: String) -> MessageBuilder<'b> {
        MessageBuilder {
            interface,
            channel,
            content,
            tts: false,
            nonce: None
        }
    }
    pub fn tts(&mut self) -> &mut Self {
        self.tts = true;
        self
    }
    pub fn nonce(&mut self, snowflake: objects::Snowflake) -> &mut Self {
        self.nonce = Some(snowflake);
        self
    }
    pub fn send(&self) -> Box<Future<Item=(), Error=Error>> {
        let mut req = self.interface.new_request(
            hyper::Method::Post,
            fut_try!(hyper::Uri::from_str(
                &format!("https://discordapp.com/api/channels/{}/messages", self.channel)).map_err(|e|Error::UhWhat(format!("URI appears to be invalid: {}", e)))));
        req.headers_mut().set(hyper::header::ContentType::json());
        req.set_body(fut_try!(serde_json::to_string(self).map_err(|e|Error::JSONError(e))));
        Box::new(self.interface.send_request(req)
            .map_err(|e|Error::HTTPError(e))
            .and_then(|res| -> Box<Future<Item=(),Error=Error>> {
                if res.status().is_success() {
                    Box::new(futures::future::ok(()))
                }
                else {
                    Box::new(res.body().concat2()
                        .map_err(|e|Error::HTTPError(e))
                        .and_then(|chunk| String::from_utf8(chunk.to_vec()).map_err(|e|Error::UnexpectedResponse(format!("Couldn't parse server response as UTF8: {}", e))))
                        .and_then(|text| futures::future::err(Error::UnexpectedResponse(format!("Failed to send message: {}", text)))))
                }
            }))
    }
}

pub struct Interface<'a> {
    sink: futures::sync::mpsc::Sender<websocket::OwnedMessage>,
    http_client: &'a hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>,
    auth_header: hyper::header::Authorization<BotAuthorizationScheme>
}

impl<'a> Interface<'a> {
    fn send_packet(&self, packet: &Packet) -> Box<Future<Item=(),Error=Error>> {
        Box::new(self.sink.clone().send(websocket::OwnedMessage::Text(
                    fut_try!(serde_json::to_string(packet).map_err(|e|Error::JSONError(e)))))
            .map(|_|()).map_err(|e|Error::UhWhat(format!("failed to send packet: {}", e))))
    }
    fn new_request<B>(&self, method: hyper::Method, uri: hyper::Uri) -> hyper::Request<B> {
        let mut tr = hyper::Request::new(method, uri);
        tr.headers_mut().set(self.auth_header.clone());
        tr
    }
    fn send_request(&self, request: hyper::Request<hyper::Body>) -> hyper::client::FutureResponse {
        self.http_client.request(request)
    }
    pub fn create_message(&self, channel_id: objects::Snowflake, content: &str) -> MessageBuilder {
        MessageBuilder::new(self, channel_id, content.to_owned())
    }
}

struct PacketHandler {
    heartbeat_interval: Option<u64>,
    event_callback: Box<Fn(events::Event, &Interface) -> ()>,
    handle: tokio_core::reactor::Handle,
    auth_header: hyper::header::Authorization<BotAuthorizationScheme>
}

impl PacketHandler {
    fn new(event_callback: Box<Fn(events::Event, &Interface) -> ()>, handle: tokio_core::reactor::Handle, auth_header: hyper::header::Authorization<BotAuthorizationScheme>) -> Self {
        return PacketHandler {
            heartbeat_interval: None,
            event_callback,
            handle,
            auth_header
        };
    }

    fn handle_message(&mut self, message: websocket::message::OwnedMessage, token: &str, sink: &futures::sync::mpsc::Sender<websocket::OwnedMessage>, http_client: &hyper::Client<hyper_tls::HttpsConnector<hyper::client::HttpConnector>>) -> Result<(), Error> {
        use websocket::message::OwnedMessage;
        println!("{:?}", message);
        match message {
            OwnedMessage::Text(ref text) => {
                let packet: Packet = serde_json::from_str(text)
                    .map_err(|e|Error::UnexpectedResponse(
                            format!("Unable to parse packet JSON: {}", e)))?;
                let auth_header = self.auth_header.clone();
                self.handle_packet(packet, token, Interface {sink: sink.clone(), http_client, auth_header})
            },
            _ => Err(Error::UnexpectedResponse(format!("Unexpected message type: {:?}", message)))
        }
    }

    fn handle_packet(&mut self, packet: Packet, token: &str, mut client: Interface) -> Result<(), Error> {
        match packet.op {
            0 => {
                let t = packet.t.ok_or(Error::UnexpectedResponse("Missing \"t\" in event dispatch".to_owned()))?;
                let event = match &t as &str {
                    "READY" => Ok(events::Event::Ready),
                    "GUILD_CREATE" => {
                        let d = &packet.d.to_string();
                        println!("{}", d);
                        let guild = serde_json::from_str(d).map_err(|e|Error::JSONError(e))?;
                        Ok(events::Event::GuildCreate(guild))
                    },
                    "MESSAGE_CREATE" => {
                        let d = &packet.d.to_string();
                        let message = serde_json::from_str(d).map_err(|e|Error::JSONError(e))?;
                        Ok(events::Event::MessageCreate(message))
                    },
                    _ => Err(Error::UnexpectedResponse(format!("Unexpected event type: {}", t)))
                }?;
                (self.event_callback)(event, &client);
                Ok(())
            },
            10 => {
                self.heartbeat_interval = Some(packet.d["heartbeat_interval"].as_u64().ok_or(Error::UnexpectedResponse("heartbeat interval isn't a number?".to_owned()))?);
                self.handle.spawn(client.send_packet(&Packet {
                    op: 2,
                    d: json!({
                        "token": token,
                        "properties": {
                            "$os": "linux",
                            "$browser": "tokio_discord",
                            "$device": "tokio_discord"
                        },
                        "compress": false
                    }),
                    s: None,
                    t: None
                }).map_err(|e|panic!(e)).map(|_|()));
                Ok(())
            },
            _ => Err(Error::UnexpectedResponse(format!("Unexpected opcode: {}", packet.op)))
        }
    }
}
