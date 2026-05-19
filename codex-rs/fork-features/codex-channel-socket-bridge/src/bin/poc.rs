use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(socket_path) = args.next() else {
        eprintln!("usage: codex-channel-socket-bridge-poc <socket> <thread-id> <text>");
        std::process::exit(2);
    };
    let Some(thread_id) = args.next() else {
        eprintln!("usage: codex-channel-socket-bridge-poc <socket> <thread-id> <text>");
        std::process::exit(2);
    };
    let text = args.collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        eprintln!("usage: codex-channel-socket-bridge-poc <socket> <thread-id> <text>");
        std::process::exit(2);
    }

    let mut stream = UnixStream::connect(socket_path).await?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": text,
            }],
        },
    });
    stream.write_all(request.to_string().as_bytes()).await?;
    stream.write_all(b"\n").await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    println!("{}", response.trim_end());
    Ok(())
}
