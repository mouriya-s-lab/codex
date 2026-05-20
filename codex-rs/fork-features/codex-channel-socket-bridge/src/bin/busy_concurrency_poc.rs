use serde_json::Value;
use serde_json::json;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(socket_path) = args.next() else {
        usage();
    };
    let Some(thread_id) = args.next() else {
        usage();
    };
    let Some(turn1_text) = args.next() else {
        usage();
    };
    let Some(turn2_text) = args.next() else {
        usage();
    };

    let stream = UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    send_turn(&mut writer, &thread_id, 1, "turn1", &turn1_text, None).await?;
    let (turn1_response, turn1_response_at_ms) = read_response(&mut lines, 1, "turn1").await?;

    let turn2_send_delta_ms = now_ms().saturating_sub(turn1_response_at_ms);
    send_turn(
        &mut writer,
        &thread_id,
        2,
        "turn2",
        &turn2_text,
        Some(turn2_send_delta_ms),
    )
    .await?;
    let _turn2_response = read_response(&mut lines, 2, "turn2").await?;

    summarize(&turn1_response);
    Ok(())
}

fn usage() -> ! {
    eprintln!(
        "usage: codex-channel-busy-concurrency-poc <socket> <thread-id> <turn1-text> <turn2-text>"
    );
    std::process::exit(2);
}

async fn send_turn(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    thread_id: &str,
    request_id: i64,
    label: &str,
    text: &str,
    delta_ms_after_previous_response: Option<u128>,
) -> std::io::Result<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": text,
            }],
        },
    });
    println!(
        "{}",
        json!({
            "tsMs": now_ms(),
            "event": "send",
            "label": label,
            "requestId": request_id,
            "deltaMsAfterPreviousResponse": delta_ms_after_previous_response,
            "text": text,
        })
    );
    writer.write_all(request.to_string().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    request_id: i64,
    label: &str,
) -> std::io::Result<(Value, u128)> {
    let Some(line) = lines.next_line().await? else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "socket closed before response",
        ));
    };
    let parsed = serde_json::from_str::<Value>(&line).unwrap_or_else(|err| {
        json!({
            "parseError": err.to_string(),
            "raw": line,
        })
    });
    let response_at_ms = now_ms();
    println!(
        "{}",
        json!({
            "tsMs": response_at_ms,
            "event": "response",
            "label": label,
            "requestId": request_id,
            "turnId": parsed.pointer("/result/turn/id"),
            "turnStatus": parsed.pointer("/result/turn/status"),
            "errorCode": parsed.pointer("/error/code"),
            "errorMessage": parsed.pointer("/error/message"),
            "raw": parsed,
        })
    );
    Ok((parsed, response_at_ms))
}

fn summarize(turn1_response: &Value) {
    println!(
        "{}",
        json!({
            "tsMs": now_ms(),
            "event": "summary",
            "turn1Started": turn1_response
                .pointer("/result/turn/status")
                .and_then(Value::as_str)
                == Some("inProgress"),
        })
    );
}

fn now_ms() -> u128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => 0,
    }
}
