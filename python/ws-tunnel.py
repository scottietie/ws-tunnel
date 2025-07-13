#!/usr/bin/env python3

import asyncio
import ssl
import argparse
import sys
from urllib.parse import urlparse
from websockets import serve
from websockets import connect
from websockets.http import Headers
from websockets.exceptions import ConnectionClosed
from websockets.asyncio.router import route
from werkzeug.routing import Map, Rule

async def stream_reader(reader: asyncio.StreamReader, queue: asyncio.Queue):
  """Reads from a stream (TCP or stdin) and puts data into a queue."""
  try:
    while True:
      data = await reader.read(4096)
      if not data:
        break
      await queue.put(data)
  except (asyncio.CancelledError, ConnectionResetError):
    pass
  finally:
    await queue.put(None)
    print("[Reader] Stream closed.", file=sys.stderr)

async def stream_writer(writer: asyncio.StreamWriter, queue: asyncio.Queue):
  """Reads from a queue and writes data to a stream (TCP or stdout)."""
  try:
    while True:
      data = await queue.get()
      if data is None:
        break
      writer.write(data)
      await writer.drain()
  except (asyncio.CancelledError, ConnectionResetError, BrokenPipeError):
    pass
  finally:
    if not writer.is_closing():
      try:
        writer.close()
        await writer.wait_closed()
      except (BrokenPipeError, ConnectionResetError):
        pass # Ignore errors when the other side hangs up
    print("[Writer] Stream closed.", file=sys.stderr)

async def pipe_streams(websocket, reader, writer, ping_interval):
  """The core logic to pipe data between a WebSocket and a stream."""
  ws_to_stream_q = asyncio.Queue(100)
  stream_to_ws_q = asyncio.Queue(100)

  reader_task = asyncio.create_task(stream_reader(reader, stream_to_ws_q))
  writer_task = asyncio.create_task(stream_writer(writer, ws_to_stream_q))

  async def ws_to_stream():
    try:
      async for message in websocket:
        await ws_to_stream_q.put(message)
    except ConnectionClosed:
      print("[WS->Stream] WebSocket connection closed.", file=sys.stderr)
    finally:
      await ws_to_stream_q.put(None)

  async def stream_to_ws():
    ping_task = None
    if ping_interval and ping_interval > 0:
      async def pinger():
        while True:
          await asyncio.sleep(ping_interval)
          await websocket.ping()
      ping_task = asyncio.create_task(pinger())

    try:
      while True:
        data = await stream_to_ws_q.get()
        if data is None:
          break
        await websocket.send(data)
    except ConnectionClosed:
      print("[Stream->WS] WebSocket connection closed.", file=sys.stderr)
    finally:
      if ping_task:
        ping_task.cancel()

  ws_read_task = asyncio.create_task(ws_to_stream())
  ws_write_task = asyncio.create_task(stream_to_ws())

  done, pending = await asyncio.wait(
      [reader_task, writer_task, ws_read_task, ws_write_task],
      return_when=asyncio.FIRST_COMPLETED
  )
  for task in pending:
    task.cancel()
  await asyncio.gather(*pending, return_exceptions=True)
  print("Connection pipeline closed.", file=sys.stderr)

# --- Server Mode ---
async def run_server(args):
  """Listens for WebSocket connections and forwards them to a TCP service."""
  async def handler(websocket):

    print(str(websocket))
    print(f"[Server] Accepted WS connection", file=sys.stderr)
    try:
      reader, writer = await asyncio.open_connection(args.forward_host, args.forward_port)
      print(f"[Server] Established forward connection to {args.forward_host}:{args.forward_port}", file=sys.stderr)
      await pipe_streams(websocket, reader, writer, None)
    except Exception as e:
      print(f"[Server] Error forwarding connection: {e}", file=sys.stderr)

  print(f"[Server] Listening on {args.listen_addr}:{args.listen_port}", file=sys.stderr)

  # TODO, add routing features
  # ws_router = route([
  #   (args.path, handler),

  #   # For any other path, the `default_handler` will be used.
  #   # The path regex ".*" matches any character sequence.
  #   # (r".*", default_handler),
  # ])

  async with serve(handler, args.listen_addr, args.listen_port):
    await asyncio.Future()

# --- Client Mode ---
async def run_client(args):
  """Connects a local stream (TCP or stdio) to a WebSocket server."""
  ssl_context = ssl.create_default_context()
  if args.ignore_cert:
    ssl_context.check_hostname = False
    ssl_context.verify_mode = ssl.CERT_NONE

  headers = Headers()
  if args.header:
    for h in args.header:
      name, value = h.split(":", 1)
      headers[name.strip()] = value.strip()

  proxy_details = {}
  if args.http_proxy:
    proxy = urlparse(args.http_proxy)
    proxy_details['http_proxy_host'] = proxy.hostname
    proxy_details['http_proxy_port'] = proxy.port

  try:
    print(f"[Client] Connecting to WebSocket at {args.url}", file=sys.stderr)
    async with connect(
      args.url,
      ssl=ssl_context if args.url.startswith("wss") else None,
      additional_headers=headers,
      **proxy_details
    ) as websocket:
      print("[Client] WebSocket connection established.", file=sys.stderr)

      if args.from_stdio:
        loop = asyncio.get_event_loop()
        reader = asyncio.StreamReader()
        protocol = asyncio.StreamReaderProtocol(reader)
        await loop.connect_read_pipe(lambda: protocol, sys.stdin)

        transport, protocol = await loop.connect_write_pipe(asyncio.streams.FlowControlMixin, sys.stdout)

        # --- FIX: Check Python version to call StreamWriter correctly ---
        if sys.version_info >= (3, 8):
            writer = asyncio.StreamWriter(transport, protocol, reader, loop)
        else:
            # Older Python versions might have different constructor needs
            # This is the most common split point
            writer = asyncio.StreamWriter(transport, protocol, reader)

        await pipe_streams(websocket, reader, writer, args.ping_interval)

      elif args.from_tcp:
        async def handle_local_conn(reader, writer):
            print(f"[Client] Accepted local TCP connection.", file=sys.stderr)
            await pipe_streams(websocket, reader, writer, args.ping_interval)
            server.close()

        server = await asyncio.start_server(handle_local_conn, '127.0.0.1', args.from_tcp)
        print(f"[Client] Listening for one local connection on 127.0.0.1:{args.from_tcp}", file=sys.stderr)
        async with server:
          await server.wait_closed()

  except Exception as e:
    print(f"[Client] Error: {e}", file=sys.stderr)

def main():
  parser = argparse.ArgumentParser(description="High-performance TCP/Pipe to WebSocket tunnel.")
  subparsers = parser.add_subparsers(dest="mode", required=True, help="Operating mode")

  client_parser = subparsers.add_parser("client", help="Connects a local stream to a WebSocket server.")
  client_group = client_parser.add_mutually_exclusive_group(required=True)
  client_group.add_argument("--from-tcp", type=int, metavar='PORT', help="Source from a local TCP port.")
  client_group.add_argument("--from-stdio", action="store_true", help="Source from stdin/stdout (for ProxyCommand).")
  client_parser.add_argument("--url", type=str, required=True, help="WebSocket URL to connect to (e.g., wss://host/path).")
  client_parser.add_argument("--ignore-cert", action="store_true", help="Ignore server's TLS certificate.")
  client_parser.add_argument("--http-proxy", type=str, metavar='URL', help="HTTP proxy to use (e.g., http://host:port).")
  client_parser.add_argument("--ping-interval", type=int, default=20, help="Interval for keep-alive pings (seconds).")
  client_parser.add_argument("--header", action="append", metavar='"Name: Value"', help="Custom header to add.")

  server_parser = subparsers.add_parser("server", help="Listens for WebSockets and forwards to a TCP service.")
  server_parser.add_argument("--listen-addr", type=str, default="0.0.0.0", help="Host address to listen on.")
  server_parser.add_argument("--listen-port", type=int, required=True, help="Port to listen on.")
  server_parser.add_argument("--forward-host", type=str, default="127.0.0.1", help="TCP host to forward traffic to.")
  server_parser.add_argument("--forward-port", type=int, required=True, help="TCP port to forward traffic to.")
  # TODO, add routing features
  # server_parser.add_argument("--path", type=str, default="/", help="URI path to accept connections on.")

  args = parser.parse_args()

  try:
    if args.mode == "client":
      asyncio.run(run_client(args))
    elif args.mode == "server":
      asyncio.run(run_server(args))
  except KeyboardInterrupt:
    print("\nExiting.", file=sys.stderr)

if __name__ == "__main__":
  main()