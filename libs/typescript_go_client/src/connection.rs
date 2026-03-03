// Copyright 2018-2026 the Deno authors. MIT license.

// Partially extracted / adapted from https://github.com/microsoft/libsyncrpc
// Copyright 2024 Microsoft Corporation. MIT license.

use std::io::BufRead;
use std::io::Result;
use std::io::Write;
use std::io::{self};

/// Lower-level wrapper around RPC-related messaging and process management.
pub struct RpcConnection<R: BufRead, W: Write> {
  reader: R,
  writer: W,
  pub bytes_read: u64,
  pub bytes_written: u64,
  pub message_count_read: u64,
  pub message_count_written: u64,
}

impl<R: BufRead, W: Write> RpcConnection<R, W> {
  pub fn new(reader: R, writer: W) -> Result<Self> {
    Ok(Self {
      reader,
      writer,
      bytes_read: 0,
      bytes_written: 0,
      message_count_read: 0,
      message_count_written: 0,
    })
  }

  pub fn write(&mut self, ty: u8, name: &[u8], payload: &[u8]) -> Result<()> {
    let w = &mut self.writer;
    rmp::encode::write_array_len(w, 3)?;
    rmp::encode::write_u8(w, ty)?;
    rmp::encode::write_bin(w, name)?;
    rmp::encode::write_bin(w, payload)?;
    w.flush()?;
    // ~10 bytes msgpack framing overhead per message
    self.bytes_written += 10 + name.len() as u64 + payload.len() as u64;
    self.message_count_written += 1;
    Ok(())
  }

  pub fn read(&mut self) -> Result<(u8, Vec<u8>, Vec<u8>)> {
    let r = &mut self.reader;
    assert_eq!(
      rmp::decode::read_array_len(r).map_err(to_io)?,
      3,
      "Message components must be a valid 3-part messagepack array."
    );
    let ty = rmp::decode::read_int(r).map_err(to_io)?;
    let name = self.read_bin()?;
    let payload = self.read_bin()?;
    // ~10 bytes msgpack framing overhead per message
    self.bytes_read += 10 + name.len() as u64 + payload.len() as u64;
    self.message_count_read += 1;
    Ok((ty, name, payload))
  }

  fn read_bin(&mut self) -> Result<Vec<u8>> {
    let r = &mut self.reader;
    let payload_len = rmp::decode::read_bin_len(r).map_err(to_io)?;
    let mut payload = vec![0u8; payload_len as usize];
    r.read_exact(&mut payload)?;
    Ok(payload)
  }

  // Helper method to create an error
  pub fn create_error(
    &self,
    name: &str,
    payload: Vec<u8>,
    expected_method: &str,
  ) -> io::Error {
    if name == expected_method {
      let payload = match String::from_utf8(payload) {
        Ok(payload) => payload,
        Err(err) => return io::Error::other(format!("{err}")),
      };
      io::Error::other(payload)
    } else {
      io::Error::other(format!(
        "name mismatch for response: expected `{expected_method}`, got `{name}`"
      ))
    }
  }
}

fn to_io<T: std::error::Error>(err: T) -> io::Error {
  io::Error::other(format!("{err}"))
}
