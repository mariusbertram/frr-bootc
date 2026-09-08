//! Direct client for FRR's vty Unix-socket protocol - the same transport
//! `vtysh` itself uses to talk to each daemon's `/var/run/frr/<daemon>.vty`
//! socket. Shared between `config-sync` (validating/applying `frr.conf`)
//! and `console` (read-only status queries), kept dependency-free so
//! neither pulls in the other's unrelated dependencies. Protocol,
//! confirmed from FRR's own `vtysh/vtysh.c` source:
//!
//! - **Request**: the raw command bytes, followed by exactly one `0x00`.
//! - **Response**: text output, followed by a 4-byte terminator of three
//!   `0x00` bytes and then one status byte (FRR's return code - `0` is
//!   success). The response text ends immediately before those three
//!   zero bytes; the terminator itself is not part of the output.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

pub struct VtyClient {
    stream: UnixStream,
}

pub struct VtyResponse {
    pub output: String,
    pub status: u8,
}

impl VtyResponse {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

impl VtyClient {
    pub fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
        Ok(VtyClient { stream })
    }

    /// Sends a single command line and reads its framed response.
    pub fn execute(&mut self, command: &str) -> io::Result<VtyResponse> {
        let mut request = Vec::with_capacity(command.len() + 1);
        request.extend_from_slice(command.as_bytes());
        request.push(0);
        self.stream.write_all(&request)?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(terminator_start) = find_terminator(&buf) {
                let status = buf[terminator_start + 3];
                let output = String::from_utf8_lossy(&buf[..terminator_start]).into_owned();
                return Ok(VtyResponse { output, status });
            }

            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "vty socket closed before sending a response terminator",
                ));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Finds the first `[0x00, 0x00, 0x00, <status>]` terminator in `buf`,
/// returning the index where the three zero bytes start.
fn find_terminator(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    buf.windows(3)
        .position(|w| w == [0, 0, 0])
        .filter(|&pos| pos + 3 < buf.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::thread;

    fn spawn_server(
        mut server: StdUnixStream,
        expected_cmd: &'static str,
        output: &'static [u8],
        status: u8,
    ) {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                let n = server.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if buf.last() == Some(&0) {
                    break;
                }
            }
            assert_eq!(&buf[..buf.len() - 1], expected_cmd.as_bytes());

            let mut response = output.to_vec();
            response.extend_from_slice(&[0, 0, 0, status]);
            server.write_all(&response).unwrap();
        });
    }

    #[test]
    fn parses_a_successful_response() {
        let (client_sock, server_sock) = StdUnixStream::pair().unwrap();
        spawn_server(server_sock, "show version", b"FRR 9.0\n", 0);

        let mut client = VtyClient {
            stream: client_sock,
        };
        let resp = client.execute("show version").unwrap();
        assert!(resp.success());
        assert_eq!(resp.output, "FRR 9.0\n");
    }

    #[test]
    fn parses_a_failure_status() {
        let (client_sock, server_sock) = StdUnixStream::pair().unwrap();
        spawn_server(server_sock, "bogus command", b"% Unknown command\n", 1);

        let mut client = VtyClient {
            stream: client_sock,
        };
        let resp = client.execute("bogus command").unwrap();
        assert!(!resp.success());
        assert_eq!(resp.status, 1);
        assert_eq!(resp.output, "% Unknown command\n");
    }

    #[test]
    fn reassembles_a_response_split_across_multiple_reads() {
        let (client_sock, mut server_sock) = StdUnixStream::pair().unwrap();

        thread::spawn(move || {
            let mut buf = [0u8; 256];
            let n = server_sock.read(&mut buf).unwrap();
            assert_eq!(&buf[..n - 1], b"router bgp 65001");

            server_sock.write_all(b"router bgp 65001\n").unwrap();
            thread::sleep(Duration::from_millis(20));
            server_sock.write_all(&[0, 0, 0, 0]).unwrap();
        });

        let mut client = VtyClient {
            stream: client_sock,
        };
        let resp = client.execute("router bgp 65001").unwrap();
        assert!(resp.success());
        assert_eq!(resp.output, "router bgp 65001\n");
    }

    #[test]
    fn empty_output_still_parses() {
        let (client_sock, server_sock) = StdUnixStream::pair().unwrap();
        spawn_server(server_sock, "end", b"", 0);

        let mut client = VtyClient {
            stream: client_sock,
        };
        let resp = client.execute("end").unwrap();
        assert!(resp.success());
        assert_eq!(resp.output, "");
    }
}
