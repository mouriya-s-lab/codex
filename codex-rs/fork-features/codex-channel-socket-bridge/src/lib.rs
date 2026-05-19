use codex_app_server_client::InProcessAppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use serde_json::json;
use std::io;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tracing::warn;

const SERVER_ERROR: i64 = -32000;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub async fn serve(handle: InProcessAppServerRequestHandle, socket_path: PathBuf) {
    if let Err(err) = run(handle, socket_path.clone()).await {
        eprintln!(
            "warn: failed to run codex channel socket bridge at {}: {err}",
            socket_path.display()
        );
        warn!(
            socket_path = %socket_path.display(),
            "failed to run codex channel socket bridge: {err}"
        );
    }
}

async fn run(handle: InProcessAppServerRequestHandle, socket_path: PathBuf) -> io::Result<()> {
    prepare_socket_path(&socket_path).await?;
    let listener = UnixListener::bind(&socket_path)?;
    set_socket_permissions(&socket_path).await?;
    let _guard = SocketFileGuard(socket_path.clone());

    loop {
        let (stream, _addr) = listener.accept().await?;
        let handle = handle.clone();
        tokio::spawn(async move {
            let _ = handle_stream(handle, stream).await;
        });
    }
}

async fn handle_stream(
    handle: InProcessAppServerRequestHandle,
    stream: UnixStream,
) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let response = handle_jsonrpc_line(&handle, &line).await;
        writer.write_all(format!("{response}\n").as_bytes()).await?;
    }
    Ok(())
}

async fn handle_jsonrpc_line(handle: &InProcessAppServerRequestHandle, line: &str) -> String {
    let request = match serde_json::from_str::<JSONRPCRequest>(line) {
        Ok(request) => request,
        Err(err) => {
            return error_response(RequestId::Integer(0), INVALID_REQUEST, err.to_string());
        }
    };
    let request_id = request.id.clone();
    let client_request = match into_client_request(request) {
        Ok(request) => request,
        Err(error) => {
            return error_response(request_id, error.code, error.message);
        }
    };
    match handle.request(client_request).await {
        Ok(Ok(result)) => json!({ "id": request_id, "result": result }).to_string(),
        Ok(Err(error)) => json!({ "id": request_id, "error": error }).to_string(),
        Err(err) => error_response(request_id, SERVER_ERROR, err.to_string()),
    }
}

fn into_client_request(request: JSONRPCRequest) -> Result<ClientRequest, JSONRPCErrorError> {
    if request.method != "turn/start" {
        return Err(JSONRPCErrorError {
            code: METHOD_NOT_FOUND,
            message: format!("unsupported bridge method `{}`", request.method),
            data: None,
        });
    }
    let value = json!({
        "id": request.id,
        "method": request.method,
        "params": request.params.unwrap_or(serde_json::Value::Null),
    });
    serde_json::from_value(value).map_err(|err| JSONRPCErrorError {
        code: INVALID_PARAMS,
        message: err.to_string(),
        data: None,
    })
}

fn error_response(id: RequestId, code: i64, message: String) -> String {
    json!({
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
    .to_string()
}

async fn prepare_socket_path(socket_path: &std::path::Path) -> io::Result<()> {
    if let Some(parent) = socket_path.parent()
        && !parent.exists()
    {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("parent directory does not exist: {}", parent.display()),
        ));
    }
    if socket_path.exists() {
        tokio::fs::remove_file(socket_path).await?;
    }
    Ok(())
}

#[cfg(unix)]
async fn set_socket_permissions(socket_path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await
}

#[cfg(not(unix))]
async fn set_socket_permissions(_socket_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

struct SocketFileGuard(PathBuf);

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                socket_path = %self.0.display(),
                "failed to remove codex channel socket: {err}"
            );
        }
    }
}
