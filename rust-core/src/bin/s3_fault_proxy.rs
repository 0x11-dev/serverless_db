use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:19100")]
    listen: String,
    #[arg(long, default_value = "127.0.0.1:9000")]
    upstream: String,
    #[arg(long, value_enum, default_value_t = FaultMode::Healthy)]
    mode: FaultMode,
    #[arg(long, value_enum, default_value_t = FaultTarget::Any)]
    target: FaultTarget,
    #[arg(long, default_value_t = 1)]
    transient_failures: usize,
    #[arg(long)]
    ready_file: Option<PathBuf>,
    #[arg(long)]
    fault_count_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FaultMode {
    Healthy,
    #[value(name = "status-503")]
    Status503,
    #[value(name = "status-429")]
    Status429,
    #[value(name = "status-408")]
    Status408,
    #[value(name = "partial-response")]
    PartialResponse,
    #[value(name = "transient-503")]
    Transient503,
    #[value(name = "transient-429")]
    Transient429,
    #[value(name = "transient-partial-response")]
    TransientPartialResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum FaultTarget {
    Any,
    #[value(name = "list-objects")]
    ListObjects,
    #[value(name = "put-object")]
    PutObject,
    #[value(name = "get-object")]
    GetObject,
    #[value(name = "head-object")]
    HeadObject,
    #[value(name = "delete-object")]
    DeleteObject,
}

#[derive(Debug)]
struct RequestHead {
    bytes: Vec<u8>,
    method: String,
    path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind(&args.listen).await?;
    if let Some(path) = &args.ready_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{}\n", args.listen))?;
    }
    eprintln!(
        "s3_fault_proxy listening on {} upstream {} mode {:?} target {:?}",
        args.listen, args.upstream, args.mode, args.target
    );

    let fault_count = Arc::new(AtomicUsize::new(0));
    loop {
        let (inbound, _) = listener.accept().await?;
        let upstream = args.upstream.clone();
        let mode = args.mode;
        let target = args.target;
        let transient_failures = args.transient_failures;
        let fault_count = fault_count.clone();
        let fault_count_file = args.fault_count_file.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(
                inbound,
                upstream,
                mode,
                target,
                transient_failures,
                fault_count,
                fault_count_file,
            )
            .await
            {
                eprintln!("s3_fault_proxy connection error: {err:#}");
            }
        });
    }
}

async fn handle_connection(
    mut inbound: TcpStream,
    upstream: String,
    mode: FaultMode,
    target: FaultTarget,
    transient_failures: usize,
    fault_count: Arc<AtomicUsize>,
    fault_count_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    if matches!(mode, FaultMode::Healthy) {
        let mut upstream = TcpStream::connect(upstream).await?;
        tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await?;
        return Ok(());
    }

    let request = read_request(&mut inbound).await?;
    let target_matches = matches_target(target, &request);
    let should_fault = match mode {
        FaultMode::Healthy => unreachable!(),
        FaultMode::Status503
        | FaultMode::Status429
        | FaultMode::Status408
        | FaultMode::PartialResponse => target_matches,
        FaultMode::Transient503 | FaultMode::Transient429 | FaultMode::TransientPartialResponse => {
            target_matches && fault_count.load(Ordering::SeqCst) < transient_failures
        }
    };

    if should_fault {
        let count = fault_count.fetch_add(1, Ordering::SeqCst) + 1;
        write_fault_count(&fault_count_file, count)?;
        match mode {
            FaultMode::Healthy => unreachable!(),
            FaultMode::Status503 | FaultMode::Transient503 => {
                write_status(&mut inbound, 503, "Service Unavailable").await?
            }
            FaultMode::Status429 | FaultMode::Transient429 => {
                write_status(&mut inbound, 429, "Too Many Requests").await?
            }
            FaultMode::Status408 => write_status(&mut inbound, 408, "Request Timeout").await?,
            FaultMode::PartialResponse | FaultMode::TransientPartialResponse => {
                write_partial_response(&mut inbound).await?
            }
        }
        inbound.shutdown().await?;
        return Ok(());
    }

    forward_one_request(&mut inbound, &upstream, request).await?;
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<RequestHead> {
    let mut buffer = [0_u8; 1024];
    let mut bytes = Vec::new();
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            anyhow::bail!("connection closed before request headers");
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            anyhow::bail!("request headers exceeded 64 KiB");
        }
    }
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .ok_or_else(|| anyhow::anyhow!("missing request header terminator"))?;
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header.lines();
    let first = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty request"))?;
    let mut first_parts = first.split_whitespace();
    let method = first_parts.next().unwrap_or_default().to_string();
    let path = first_parts.next().unwrap_or_default().to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let already_read_body = bytes.len().saturating_sub(header_end);
    if already_read_body < content_length {
        let mut rest = vec![0_u8; content_length - already_read_body];
        stream.read_exact(&mut rest).await?;
        bytes.extend_from_slice(&rest);
    }
    Ok(RequestHead {
        bytes,
        method,
        path,
    })
}

async fn forward_one_request(
    inbound: &mut TcpStream,
    upstream_addr: &str,
    mut request: RequestHead,
) -> anyhow::Result<()> {
    force_connection_close(&mut request.bytes);
    let mut upstream = TcpStream::connect(upstream_addr).await?;
    upstream.write_all(&request.bytes).await?;
    let mut response = Vec::new();
    upstream.read_to_end(&mut response).await?;
    inbound.write_all(&response).await?;
    inbound.shutdown().await?;
    Ok(())
}

fn force_connection_close(request: &mut Vec<u8>) {
    if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
        let insert_at = header_end + 2;
        request.splice(
            insert_at..insert_at,
            b"connection: close\r\n".iter().copied(),
        );
    }
}

fn matches_target(target: FaultTarget, request: &RequestHead) -> bool {
    if target == FaultTarget::Any {
        return true;
    }
    match request.method.as_str() {
        "PUT" => target == FaultTarget::PutObject,
        "HEAD" => target == FaultTarget::HeadObject,
        "DELETE" => target == FaultTarget::DeleteObject,
        "GET" if request.path.contains("list-type=2") => target == FaultTarget::ListObjects,
        "GET" => target == FaultTarget::GetObject,
        _ => false,
    }
}

fn write_fault_count(path: &Option<PathBuf>, count: usize) -> anyhow::Result<()> {
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("{count}\n"))?;
    }
    Ok(())
}

async fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> anyhow::Result<()> {
    let error_code = match code {
        408 => "RequestTimeout",
        429 => "SlowDown",
        503 => "ServiceUnavailable",
        _ => "InternalError",
    };
    let body = format!(
        "<Error><Code>{error_code}</Code><Message>{reason}</Message><RequestId>sdb-fault</RequestId></Error>"
    );
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/xml\r\ncontent-length: {}\r\nconnection: close\r\nx-amz-request-id: sdb-fault\r\nx-sdb-fault: status\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

async fn write_partial_response(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-length: 1024\r\nconnection: close\r\ncontent-type: application/octet-stream\r\nx-sdb-fault: partial-response\r\n\r\npartial",
        )
        .await?;
    Ok(())
}
