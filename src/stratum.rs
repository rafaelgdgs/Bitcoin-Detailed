use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 8;

pub struct Stratum {
    socket: TcpStream,
    incoming: Receiver<Result<Value>>,
    next_id: u64,
}

impl Stratum {
    pub fn connect(endpoint: &str) -> Result<Self> {
        let mut addresses = endpoint.to_socket_addrs()?.peekable();
        ensure!(
            addresses.peek().is_some(),
            "The pool hostname did not resolve to an IP address"
        );
        let mut last_error = None;
        for address in addresses.take(4) {
            match TcpStream::connect_timeout(&address, Duration::from_secs(3)) {
                Ok(socket) => {
                    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
                    socket.set_nodelay(true)?;
                    let reader = socket.try_clone()?;
                    let (sender, incoming) = mpsc::sync_channel(QUEUE_CAPACITY);
                    thread::Builder::new()
                        .name("stratum-reader".into())
                        .spawn(move || {
                            let mut reader = BufReader::new(reader);
                            let mut line = Vec::new();
                            loop {
                                let message = read_message(&mut reader, &mut line);
                                let failed = message.is_err();
                                if sender.send(message).is_err() || failed {
                                    break;
                                }
                            }
                        })?;
                    return Ok(Self {
                        socket,
                        incoming,
                        next_id: 1,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap()).context("Could not connect to the pool")
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    pub fn reply(&mut self, id: &Value, result: Value, error: Value) -> Result<()> {
        self.send(&json!({"id": id, "result": result, "error": error}))
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let line = format!("{message}\n");
        self.socket
            .write_all(line.as_bytes())
            .context("Failed to send Stratum message")
    }

    pub fn read(&self, timeout: Duration) -> Vec<Result<Value>> {
        match self.incoming.recv_timeout(timeout) {
            Ok(first) => std::iter::once(first)
                .chain(self.incoming.try_iter().take(QUEUE_CAPACITY - 1))
                .collect(),
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => vec![Err(anyhow!("The Stratum reader stopped"))],
        }
    }
}

impl Drop for Stratum {
    fn drop(&mut self) {
        // Wake the reader even if the pool is idle.
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn read_message(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Result<Value> {
    loop {
        line.clear();
        let length = reader
            .take((MAX_MESSAGE + 1) as u64)
            .read_until(b'\n', line)
            .context("Failed to read Stratum message")?;
        ensure!(length > 0, "The pool closed the connection");
        ensure!(length <= MAX_MESSAGE, "Stratum message is too long");
        ensure!(
            line.ends_with(b"\n"),
            "The pool closed the connection mid-message"
        );
        if !line.iter().all(u8::is_ascii_whitespace) {
            return serde_json::from_slice(line).context("Invalid Stratum JSON");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stratum_lines_can_span_reads_without_losing_message_order() -> Result<()> {
        let mut reader =
            BufReader::with_capacity(3, &b" {\"id\":1}\r\n\n{\"id\":2}\n{\"id\":3}"[..]);
        let mut line = Vec::new();
        assert_eq!(read_message(&mut reader, &mut line)?, json!({"id": 1}));
        assert_eq!(read_message(&mut reader, &mut line)?, json!({"id": 2}));
        let error = read_message(&mut reader, &mut line).unwrap_err();
        assert_eq!(
            error.to_string(),
            "The pool closed the connection mid-message"
        );
        Ok(())
    }
}
