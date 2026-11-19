use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

const MAX_MESSAGE: usize = 4 * 1024 * 1024;

pub struct Stratum {
    socket: TcpStream,
    buffer: Vec<u8>,
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
                    socket.set_read_timeout(Some(Duration::from_millis(10)))?;
                    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
                    socket.set_nodelay(true)?;
                    return Ok(Self {
                        socket,
                        buffer: Vec::new(),
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

    pub fn read(&mut self) -> Result<Vec<Value>> {
        let mut chunk = [0; 8192];
        match self.socket.read(&mut chunk) {
            Ok(0) => bail!("The pool closed the connection"),
            Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error).context("Failed to read Stratum message"),
        }
        ensure!(
            self.buffer.len() <= MAX_MESSAGE,
            "Stratum message is too long"
        );
        let Some(end) = self.buffer.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let lines = self.buffer.drain(..=end).collect::<Vec<_>>();
        lines
            .split(|&b| b == b'\n')
            .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
            .map(|line| serde_json::from_slice(line).context("Invalid Stratum JSON"))
            .collect()
    }
}
