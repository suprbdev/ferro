use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub struct Client {
    stream: BufReader<TcpStream>,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

async fn write_request(
    stream: &mut BufReader<TcpStream>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> io::Result<()> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: chat\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => head.push_str(&format!(
            "content-type: application/json\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )),
        None => head.push_str("\r\n"),
    }
    let mut bytes = head.into_bytes();
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }
    stream.get_mut().write_all(&bytes).await
}

async fn read_head(stream: &mut BufReader<TcpStream>) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut line = String::new();
    if stream.read_line(&mut line).await? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| bad("status line"))?;
    let mut headers = Vec::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Ok((status, headers));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
}

impl Client {
    pub async fn connect(addr: SocketAddr) -> io::Result<Client> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Client {
            stream: BufReader::new(stream),
        })
    }

    pub async fn send(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> io::Result<Response> {
        write_request(&mut self.stream, method, path, headers, body).await?;
        let (status, headers) = read_head(&mut self.stream).await?;
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        self.stream.read_exact(&mut body).await?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    pub async fn get(&mut self, path: &str) -> io::Result<Response> {
        self.send("GET", path, &[], None).await
    }

    pub async fn json(
        &mut self,
        method: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> io::Result<Response> {
        let bytes = body.to_string().into_bytes();
        self.send(method, path, &[], Some(&bytes)).await
    }

    pub async fn act(
        &mut self,
        user: u64,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> io::Result<Response> {
        let id = user.to_string();
        let bytes = body.map(|body| body.to_string().into_bytes());
        self.send(method, path, &[("x-user", id.as_str())], bytes.as_deref())
            .await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: String,
    pub id: Option<String>,
    pub data: String,
}

pub struct SseStream {
    stream: BufReader<TcpStream>,
    pending: String,
    pub status: u16,
}

impl SseStream {
    pub async fn open(
        addr: SocketAddr,
        path: &str,
        headers: &[(&str, &str)],
    ) -> io::Result<SseStream> {
        let stream = TcpStream::connect(addr).await?;
        let mut stream = BufReader::new(stream);
        write_request(&mut stream, "GET", path, headers, None).await?;
        let (status, _) = read_head(&mut stream).await?;
        Ok(SseStream {
            stream,
            pending: String::new(),
            status,
        })
    }

    async fn chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut line = String::new();
        if self.stream.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        let size = usize::from_str_radix(line.trim(), 16).map_err(|_| bad("chunk size"))?;
        if size == 0 {
            return Ok(None);
        }
        let mut data = vec![0u8; size + 2];
        self.stream.read_exact(&mut data).await?;
        data.truncate(size);
        Ok(Some(data))
    }

    pub async fn next(&mut self) -> io::Result<Option<SseEvent>> {
        loop {
            if let Some(end) = self.pending.find("\n\n") {
                let block: String = self.pending.drain(..end + 2).collect();
                let mut event = SseEvent {
                    event: "message".into(),
                    id: None,
                    data: String::new(),
                };
                let mut seen = false;
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        event.event = value.trim().to_string();
                        seen = true;
                    } else if let Some(value) = line.strip_prefix("id:") {
                        event.id = Some(value.trim().to_string());
                        seen = true;
                    } else if let Some(value) = line.strip_prefix("data:") {
                        event.data.push_str(value.trim_start());
                        seen = true;
                    }
                }
                if seen {
                    return Ok(Some(event));
                }
                continue;
            }
            match self.chunk().await? {
                Some(bytes) => self.pending.push_str(&String::from_utf8_lossy(&bytes)),
                None => return Ok(None),
            }
        }
    }
}
