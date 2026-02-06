use std::io;
use std::net::{SocketAddr, ToSocketAddrs};

use clap::{Arg, value_parser};
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  net::{TcpListener, TcpStream},
  sync::mpsc,
};

const IO_CHUNK_SIZE: usize = 256 * 1024;
const PIPELINE_DEPTH: usize = 64;
const LISTEN_BACKLOG: i32 = 4096;
const SOCKET_BUFFER_SIZE: usize = 4 * 1024 * 1024;

type EchoChunk = (Vec<u8>, usize);

fn resolve_bind_addr(host: &str, port: u16) -> io::Result<SocketAddr> {
  (host, port).to_socket_addrs()?.next().ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidInput, "no address resolved")
  })
}

fn create_listener(addr: SocketAddr) -> io::Result<TcpListener> {
  let domain = if addr.is_ipv4() {
    Domain::IPV4
  } else {
    Domain::IPV6
  };
  let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
  #[cfg(not(windows))]
  socket.set_reuse_address(true)?;
  #[cfg(not(windows))]
  let _ = socket.set_reuse_port(true);
  socket.set_nonblocking(true)?;
  socket.bind(&addr.into())?;
  socket.listen(LISTEN_BACKLOG)?;
  let std_listener: std::net::TcpListener = socket.into();
  TcpListener::from_std(std_listener)
}

fn tune_stream(socket: &TcpStream) -> io::Result<()> {
  socket.set_nodelay(true)?;
  let sock_ref = SockRef::from(socket);
  let _ = sock_ref.set_recv_buffer_size(SOCKET_BUFFER_SIZE);
  let _ = sock_ref.set_send_buffer_size(SOCKET_BUFFER_SIZE);
  Ok(())
}

async fn handle_echo_client(mut socket: TcpStream) -> io::Result<()> {
  tune_stream(&socket)?;
  let (mut reader, mut writer) = socket.split();

  let (chunk_tx, mut chunk_rx) = mpsc::channel::<EchoChunk>(PIPELINE_DEPTH);
  let (recycle_tx, mut recycle_rx) = mpsc::channel::<Vec<u8>>(PIPELINE_DEPTH);
  for _ in 0..PIPELINE_DEPTH {
    recycle_tx
      .try_send(vec![0; IO_CHUNK_SIZE])
      .expect("recycle queue should not be full during prefill");
  }

  let read_loop = async {
    loop {
      let mut chunk = match recycle_rx.recv().await {
        Some(chunk) => chunk,
        None => return Ok(()),
      };

      let n = reader.read(&mut chunk).await?;
      if n == 0 {
        break;
      }
      chunk_tx.send((chunk, n)).await.map_err(|_| {
        io::Error::new(
          io::ErrorKind::BrokenPipe,
          "write pipeline closed before read completed",
        )
      })?;
    }
    Ok(())
  };

  let write_loop = async {
    while let Some((chunk, n)) = chunk_rx.recv().await {
      writer.write_all(&chunk[..n]).await?;
      if recycle_tx.send(chunk).await.is_err() {
        break;
      }
    }
    writer.shutdown().await
  };

  tokio::try_join!(read_loop, write_loop)?;
  Ok(())
}

async fn run_echo_server(host: String, port: u16) -> io::Result<()> {
  let bind_addr = resolve_bind_addr(&host, port)?;
  let listener = create_listener(bind_addr)?;
  println!("Echo server listening on {}", bind_addr);

  loop {
    let (socket, _) = listener.accept().await?;

    tokio::spawn(async move {
      let _ = handle_echo_client(socket).await;
    });
  }
}

#[tokio::main]
async fn main() -> io::Result<()> {
  let matches = clap::Command::new("echo_server")
    .about("TCP echo server")
    .arg(
      Arg::new("host")
        .short('H')
        .long("host")
        .default_value("127.0.0.1"),
    )
    .arg(
      Arg::new("port")
        .short('p')
        .long("port")
        .default_value("3002")
        .value_parser(value_parser!(u16)),
    )
    .get_matches();

  let host = matches.get_one::<String>("host").unwrap();
  let port = matches.get_one::<u16>("port").unwrap();

  run_echo_server(host.to_string(), *port).await
}
