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

pub enum Error {
    HTTPError(hyper::Error),
    TLSError(native_tls::Error),
    WebsocketError(websocket::WebSocketError),
    AuthenticationFailed,
    UnexpectedResponse(String)
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
            Error::AuthenticationFailed => "Authentication Failed",
            Error::UnexpectedResponse(ref msg) => &msg
        }
    }
}

type WebSocket = websocket::client::async::Client<Box<websocket::stream::async::Stream + Send>>;

enum ConnectionState {
    Disconnected,
    Connecting,
    Connected(WebSocket),
    Ready(WebSocket),
    Failed(Error)
}

#[derive(Debug, Clone)]
struct BotAuthorizationScheme {
    token: String
}

impl FromStr for BotAuthorizationScheme {
    type Err = Error;
    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Ok(BotAuthorizationScheme {
            token: token.to_owned()
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
    connection: ConnectionState
}

impl Client {
    pub fn login_bot(handle: &tokio_core::reactor::Handle, token: &str) -> Box<Future<Item=Client, Error=Error>> {
        let handle = handle.clone();
        let http = hyper::Client::configure()
            .connector(fut_try!(hyper_tls::HttpsConnector::new(1, &handle).map_err(|e|Error::TLSError(e))))
            .build(&handle);
        let mut request = hyper::Request::new(
            hyper::Method::Get,
            fut_try!(hyper::Uri::from_str("https://discordapp.com/api/v6/gateway/bot").map_err(|e|Error::HTTPError(e.into())))
            );
        request.headers_mut().set(
            hyper::header::Authorization(
                fut_try!(BotAuthorizationScheme::from_str(token))));
        Box::new(http.request(request)
                 .map_err(|e|Error::HTTPError(e))
           .and_then(|response| {
                println!("{:?}", response);
                let status = response.status();
                if status == hyper::StatusCode::Unauthorized {
                    return Err(Error::AuthenticationFailed);
                }
                if status == hyper::StatusCode::Ok {
                    return Ok(response.body());
                }
                Err(Error::UnexpectedResponse(format!("Gateway request responded with unexpected status {}", status)))
           })
           .and_then(|body| body.concat2().map_err(|e|Error::HTTPError(e)))
           .and_then(|chunk| -> Box<futures::future::Future<Item=Client, Error=Error>> {
               let value: serde_json::Value = match serde_json::from_slice(&chunk) {
                   Ok(value) => value,
                   Err(err) => return Box::new(futures::future::err(
                       Error::UnexpectedResponse(
                           format!(
                               "Unable to parse gateway API response: {:?}", err))))
               };
               println!("{}", value);
               let client = Client::new(handle, value["url"].to_string());
               Box::new(client.connect()
                   .map(|_|client))
           }))
    }
    fn new(handle: tokio_core::reactor::Handle, gateway_url: String) -> Self {
        Client {
            handle,
            gateway_url,
            connection: ConnectionState::Disconnected
        }
    }
    fn connect(&mut self) -> Box<futures::future::Future<Item=(), Error=Error>> {
        if match self.connection {
            ConnectionState::Disconnected => true,
            ConnectionState::Connecting => false,
            ConnectionState::Connected(_) => return Box::new(futures::future::ok(())),
            ConnectionState::Failed(_) => true
        } {
            self.connection = ConnectionState::Connecting;
            self.handle.spawn(fut_try!(
                    websocket::ClientBuilder::new(&format!("{}?v=6&encoding=json", self.gateway_url))
                    .map_err(|err| Error::UnexpectedResponse(
                            format!("Unable to parse gateway URI: {}", err)
                            )))
                .async_connect(None, &self.handle)
                .and_then(|(socket, _)| {
                    self.handle.spawn(socket.for_each(|packet|self.handle_packet(packet)).map_err(|e|Error::WebsocketError(e)));
                }));
        }
        Box::new(futures::future::err(Error::UnexpectedResponse("TODO make this work".to_owned())))
    }
    fn handle_packet(&mut self, message: websocket::message::OwnedMessage) -> Result<(), Error> {
        println!("{:?}", message);
        Ok(())
    }
}
