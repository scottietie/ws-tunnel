// FIX: Removed unused `connect_async_with_config`
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message,
    tungstenite::http::{Request, Response, Uri},
    client_async_with_config, MaybeTlsStream,
};
// FIX: Removed unused `TlsStream` import.
use tokio_native_tls::{TlsConnector as TokioTlsConnector};

// ... (Cli, Commands, ClientArgs, ServerArgs, parse_key_val structs are unchanged) ...
use clap::{Parser, Subcommand};
use futures_util::{stream::StreamExt, SinkExt};
use std::time::Duration;
use tokio::{
  io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
  net::{TcpListener, TcpStream},
  sync::mpsc,
};

/// A high-performance TCP/Pipe to WebSocket tunnel.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Cli {
  #[clap(subcommand)]
  command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
  /// Client mode: Connects a local stream (TCP or stdio) to a WebSocket server.
  /// Ideal for use with SSH ProxyCommand.
  Client(ClientArgs),
  /// Server mode: Listens for WebSocket connections and forwards them to a TCP service.
  /// Runs as a daemon on a remote server.
  Server(ServerArgs),
}

#[derive(Parser, Debug)]
struct ClientArgs {
  /// The WebSocket URL to connect to (e.g., wss://example.com/tunnel).
  #[clap(short, long)]
  url: String,

  /// Source for the data stream: a local TCP port.
  #[clap(long, group = "input")]
  from_tcp: Option<u16>,

  /// Source for the data stream: standard input/output.
  /// Use this for SSH ProxyCommand.
  #[clap(long, group = "input")]
  from_stdio: bool,

  /// Ignore server's TLS certificate validation errors.
  #[clap(long)]
  ignore_cert: bool,

  /// HTTP proxy to use for the connection (e.g., http://proxy.example.com:8080).
  #[clap(long)]
  http_proxy: Option<String>,

  /// Interval in seconds to send keep-alive pings.
  #[clap(long, default_value_t = 30)]
  ping_interval: u64,

  /// Custom header to add to the handshake (can be used multiple times).
  #[clap(long, short = 'H', value_parser = parse_key_val, action = clap::ArgAction::Append)]
  header: Vec<(String, String)>,
}

#[derive(Parser, Debug)]
struct ServerArgs {
  /// The host and port to listen on for WebSocket connections (e.g., 0.0.0.0:8080).
  #[clap(short, long, default_value = "0.0.0.0:8080")]
  listen_addr: String,

  /// The TCP address to forward traffic to (e.g., 127.0.0.1:22 for SSH).
  #[clap(short, long, default_value = "127.0.0.1:22")]
  forward_to: String,

  /// The URI path to accept connections on (e.g., /tunnel).
  #[clap(long, default_value = "/")]
  path: String,
}

/// Helper to parse "Key:Value" pairs for headers.
fn parse_key_val(s: &str) -> Result<(String, String), String> {
  s.split_once(':')
    .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
    .ok_or_else(|| format!("invalid KEY:VALUE format: '{}'", s))
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let cli = Cli::parse();
  match cli.command {
    Commands::Client(args) => run_client(args).await,
    Commands::Server(args) => run_server(args).await,
  }
}

// --- Server Mode Implementation ---

async fn run_server(args: ServerArgs) -> Result<(), Box<dyn std::error::Error>> {
  let listener = TcpListener::bind(&args.listen_addr).await?;
  println!("[Server] Listening on {}", args.listen_addr);

  while let Ok((stream, addr)) = listener.accept().await {
    println!("[Server] Accepted connection from: {}", addr);
    let args_clone = args.forward_to.clone();
    let path_clone = args.path.clone();
    tokio::spawn(async move {
      let http_callback = |req: &Request<()>, res: Response<()>| {
        if req.uri().path() == path_clone {
          println!("[Server] Handshake validated for path: {}", req.uri());
          Ok(res)
        } else {
          println!("[Server] Rejected handshake for path: {}", req.uri());
          // FIX: Removed the `mut` from `builder` as it's not needed.
          let builder = Response::builder();
          let err_res = builder.status(404).body(None).expect("Failed to build 404 response");
          Err(err_res)
        }
      };

      match tokio_tungstenite::accept_hdr_async(stream, http_callback).await {
        Ok(ws_stream) => {
          println!("[Server] WebSocket handshake successful with {}", addr);
          match TcpStream::connect(&args_clone).await {
            Ok(tcp_stream) => {
              println!("[Server] Forwarding to {}", args_clone);
              let (tcp_reader, tcp_writer) = tcp_stream.into_split();
              pipe_streams(ws_stream, tcp_reader, tcp_writer, None).await;
            }
            Err(e) => eprintln!("[Server] Failed to connect to forward address: {}", e),
          }
        }
        Err(e) => eprintln!("[Server] WebSocket handshake failed with {}: {}", addr, e),
      }
    });
  }
  Ok(())
}

// --- Client Mode Implementation ---

async fn run_client(args: ClientArgs) -> Result<(), Box<dyn std::error::Error>> {
  if args.from_stdio {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    connect_and_pipe(args, stdin, stdout).await?;
  } else if let Some(port) = args.from_tcp {
    let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
    println!("[Client] Listening for a local connection on port {}", port);
    let (tcp_stream, addr) = listener.accept().await?;
    println!("[Client] Accepted local connection from {}", addr);
    let (reader, writer) = tcp_stream.into_split();
    connect_and_pipe(args, reader, writer).await?;
  } else {
    eprintln!("[Client] Error: No input source specified. Use --from-stdio or --from-tcp.");
    std::process::exit(1);
  }
  Ok(())
}

async fn connect_and_pipe<R, W>(
  args: ClientArgs,
  reader: R,
  writer: W,
) -> Result<(), Box<dyn std::error::Error>>
where
  R: AsyncRead + Unpin + Send + 'static,
  W: AsyncWrite + Unpin + Send + 'static,
{
  let uri: Uri = args.url.parse()?;
  let host = uri.host().ok_or("No host in URL")?.to_string();
  let is_secure = uri.scheme_str() == Some("wss");

  let mut request_builder = Request::builder().uri(&uri);
  for (key, value) in &args.header {
    request_builder = request_builder.header(key, value);
  }
  let request = request_builder.body(())?;

  let ws_stream = if let Some(proxy_url) = &args.http_proxy {
    let proxy_uri: Uri = proxy_url.parse()?;
    let proxy_addr = format!(
      "{}:{}",
      proxy_uri.host().ok_or("No host in proxy URI")?,
      proxy_uri.port_u16().unwrap_or(80)
    );
    let mut proxy_stream = TcpStream::connect(proxy_addr).await?;

    if is_secure {
      let connect_req = format!(
        "CONNECT {}:{} HTTP/1.1\r\nHost: {}:{}\r\n\r\n",
        host, uri.port_u16().unwrap_or(443),
        host, uri.port_u16().unwrap_or(443)
      );
      proxy_stream.write_all(connect_req.as_bytes()).await?;

      let mut buf = [0; 1024];
      let n = proxy_stream.read(&mut buf).await?;
      if !String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200") {
        return Err("Proxy CONNECT request failed".into());
      }
    }

    let maybe_tls_stream = if is_secure {
      let mut tls_builder = native_tls::TlsConnector::builder();
      if args.ignore_cert {
          tls_builder.danger_accept_invalid_certs(true);
      }

      // 1. Build the native_tls connector
      let native_connector = tls_builder.build()?;

      // 2. Wrap it with the tokio-compatible connector and assign it
      //    to the variable named `tls_connector`
      let tls_connector = TokioTlsConnector::from(native_connector);

      // 3. NOW you can use the `tls_connector` variable
      let tls_stream = tls_connector.connect(&host, proxy_stream).await?;

      MaybeTlsStream::NativeTls(tls_stream)
    } else {
      MaybeTlsStream::Plain(proxy_stream)
    };

    // FIX: Use `client_async_with_config` for connecting with an existing stream.
    let (ws, _) = client_async_with_config(request, maybe_tls_stream, None).await?;
    ws

  } else {
    // FIX: Use the simpler `connect_async` for direct connections. It's less error-prone.
    let (ws, _) = connect_async(request).await?;
    ws
  };

  println!("[Client] WebSocket connection established.");
  pipe_streams(ws_stream, reader, writer, Some(args.ping_interval)).await;
  Ok(())
}

// --- Core Data Piping Logic (Unchanged but validated) ---

async fn pipe_streams<S, R, W>(
  ws_stream: S,
  mut tcp_reader: R,
  mut tcp_writer: W,
  ping_interval: Option<u64>,
) where
  S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
    + futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
    + Unpin
    + Send
    + 'static,
  R: AsyncRead + Unpin + Send + 'static,
  W: AsyncWrite + Unpin + Send + 'static,
{
  let (mut ws_sink, mut ws_stream) = ws_stream.split();

  let (tcp_to_ws_tx, mut tcp_to_ws_rx) = mpsc::channel::<Vec<u8>>(100);
  let (ws_to_tcp_tx, mut ws_to_tcp_rx) = mpsc::channel::<Vec<u8>>(100);

  // Task: Read from TCP/stdio, send to channel
  let tcp_read_task = tokio::spawn(async move {
    let mut buf = vec![0; 4096];
    while let Ok(n) = tcp_reader.read(&mut buf).await {
      if n == 0 { break; }
      if tcp_to_ws_tx.send(buf[..n].to_vec()).await.is_err() { break; }
    }
  });

  // Task: Read from channel, write to TCP/stdio
  let tcp_write_task = tokio::spawn(async move {
    while let Some(data) = ws_to_tcp_rx.recv().await {
      if tcp_writer.write_all(&data).await.is_err() { break; }
    }
  });

  // Task: Read from WebSocket, send to channel
  let ws_read_task = tokio::spawn(async move {
    while let Some(Ok(msg)) = ws_stream.next().await {
        match msg {
            Message::Binary(data) => {
                if ws_to_tcp_tx.send(data).await.is_err() { break; }
            }
            Message::Close(_) => {
                break;
            }
            // Ignore Ping, Pong, Text messages
            _ => {}
        }
    }
  });

  // Task: Read from channel, write to WebSocket (with ping logic)
  let ws_write_task = tokio::spawn(async move {
    let mut interval = if let Some(secs) = ping_interval {
        if secs > 0 { Some(tokio::time::interval(Duration::from_secs(secs))) } else { None }
    } else { None };

    loop {
      tokio::select! {
        biased;
        Some(data) = tcp_to_ws_rx.recv() => {
          if ws_sink.send(Message::Binary(data)).await.is_err() { break; }
        }
        _ = async { if let Some(ref mut i) = interval { i.tick().await; } else { std::future::pending().await } }, if interval.is_some() => {
          if ws_sink.send(Message::Ping(vec![])).await.is_err() { break; }
        }
        else => break,
      }
    }
  });

  tokio::select! {
      _ = tcp_read_task => {},
      _ = tcp_write_task => {},
      _ = ws_read_task => {},
      _ = ws_write_task => {},
  }
  println!("Connection closed.");
}