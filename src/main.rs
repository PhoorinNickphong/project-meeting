mod storage;

use serde::{Serialize, de::DeserializeOwned};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const INDEX: &str = include_str!("../ui/index.html");
const CSS: &str = include_str!("../ui/app.css");
const JS: &str = include_str!("../ui/app.js");
const WORKLET: &str = include_str!("../ui/capture.js");
const RECORDING_TIMEOUT: Duration = Duration::from_secs(30);

struct ActiveRecording {
    id: String,
    owner_key: String,
    last_seen: Instant,
}

#[derive(Serialize)]
struct MeetingStatus {
    #[serde(flatten)]
    meeting: storage::Meeting,
    recording: bool,
}

fn recording_conflict(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, message)
}

fn recording_access(
    active: &mut Option<ActiveRecording>,
    request: &Request,
    id: &str,
) -> io::Result<()> {
    let recording = active
        .as_mut()
        .filter(|recording| recording.id == id)
        .ok_or_else(|| recording_conflict("การอัดหยุดแล้ว เปิดรายการประชุมเพื่อกู้เสียง"))?;
    if request_header(request, "X-Recording-Key") != Some(recording.owner_key.as_str()) {
        return Err(recording_conflict("การอัดนี้อยู่ในอีกหน้าต่างหนึ่ง"));
    }
    recording.last_seen = Instant::now();
    Ok(())
}

fn header(key: &str, value: &str) -> Header {
    Header::from_bytes(key, value).expect("internal response header is valid")
}

fn request_header<'a>(request: &'a Request, key: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(key))
        .map(|header| header.value.as_str())
}

fn respond<R: Read>(request: Request, response: Response<R>, mime: &str) {
    let _ = request.respond(
        response
            .with_header(header("Content-Type", mime))
            .with_header(header("Cache-Control", "no-store"))
            .with_header(header("Referrer-Policy", "no-referrer"))
            .with_header(header("X-Content-Type-Options", "nosniff"))
            .with_header(header(
                "Content-Security-Policy",
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; media-src 'self' blob:; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            )),
    );
}

fn json(request: Request, result: io::Result<impl Serialize>) {
    let (status, body) = match result.and_then(|value| Ok(serde_json::to_string(&value)?)) {
        Ok(body) => (200, body),
        Err(error) => match error.kind() {
            io::ErrorKind::NotFound => (404, r#"{"error":"ไม่พบการประชุม"}"#.into()),
            io::ErrorKind::WouldBlock => (
                409,
                serde_json::json!({"error": error.to_string()}).to_string(),
            ),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => (
                400,
                serde_json::json!({"error": error.to_string()}).to_string(),
            ),
            _ => {
                eprintln!("{error}");
                (
                    500,
                    r#"{"error":"อ่านหรือบันทึกไฟล์ไม่ได้ ตรวจสอบพื้นที่ดิสก์และลองอีกครั้ง"}"#.into(),
                )
            }
        },
    };
    respond(
        request,
        Response::from_string(body).with_status_code(status),
        "application/json; charset=utf-8",
    );
}

fn read_bytes(request: &mut Request, max: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    request.as_reader().take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ข้อมูลมีขนาดใหญ่เกินไป",
        ));
    }
    Ok(bytes)
}

fn read_json<T: DeserializeOwned>(request: &mut Request) -> io::Result<T> {
    serde_json::from_slice(&read_bytes(request, 16_384)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ข้อมูลไม่ถูกต้อง"))
}

fn audio_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        return (suffix > 0).then(|| (size.saturating_sub(suffix), size - 1));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end).then_some((start, end))
}

fn send_audio(request: Request, root: &Path, id: &str) {
    let result = storage::audio(root, id).and_then(|file| Ok((file.metadata()?.len(), file)));
    let (size, mut file) = match result {
        Ok(value) => value,
        Err(error) => return json(request, Err::<(), _>(error)),
    };
    let range = request_header(&request, "Range");
    let (status, start, end) = match range {
        None => (200, 0, size - 1),
        Some(value) => match audio_range(value, size) {
            Some((start, end)) => (206, start, end),
            None => {
                let response = Response::empty(StatusCode(416))
                    .with_header(header("Content-Range", &format!("bytes */{size}")));
                return respond(request, response, "audio/wav");
            }
        },
    };
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return json(request, Err::<(), _>(error));
    }
    let count = end - start + 1;
    let mut response = Response::new(
        StatusCode(status),
        vec![header("Accept-Ranges", "bytes")],
        file.take(count),
        Some(count as usize),
        None,
    );
    if status == 206 {
        response = response.with_header(header(
            "Content-Range",
            &format!("bytes {start}-{end}/{size}"),
        ));
    }
    respond(request, response, "audio/wav");
}

fn handle(
    mut request: Request,
    root: &Path,
    host: &str,
    key: &str,
    active: &mut Option<ActiveRecording>,
) -> bool {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = request.method().clone();
    if request_header(&request, "Host") != Some(host) {
        let _ = request.respond(Response::empty(StatusCode(403)));
        return true;
    }
    if method == Method::Get {
        let asset = match path {
            "/" => Some((
                INDEX.replace("__MEETING_KEY__", key),
                "text/html; charset=utf-8",
            )),
            "/app.css" => Some((CSS.to_owned(), "text/css; charset=utf-8")),
            "/app.js" => Some((JS.to_owned(), "text/javascript; charset=utf-8")),
            "/capture.js" => Some((WORKLET.to_owned(), "text/javascript; charset=utf-8")),
            _ => None,
        };
        if let Some((content, mime)) = asset {
            respond(request, Response::from_string(content), mime);
            return true;
        }
    }
    let media_request = method == Method::Get && path.ends_with("/audio");
    if !(request_header(&request, "X-Meeting-Key") == Some(key)
        || (media_request && query.strip_prefix("key=") == Some(key)))
    {
        let _ = request.respond(Response::empty(StatusCode(403)));
        return true;
    }
    if active
        .as_ref()
        .is_some_and(|recording| recording.last_seen.elapsed() >= RECORDING_TIMEOUT)
    {
        *active = None;
    }
    match (&method, path) {
        (&Method::Get, "/api/meetings") => json(
            request,
            storage::list(root).map(|meetings| {
                meetings
                    .into_iter()
                    .map(|meeting| MeetingStatus {
                        recording: active.as_ref().is_some_and(|entry| entry.id == meeting.id),
                        meeting,
                    })
                    .collect::<Vec<_>>()
            }),
        ),
        (&Method::Get, "/api/config") => json(request, Ok(serde_json::json!({"directory": root}))),
        (&Method::Post, "/api/start") => {
            let result = (|| {
                if active.is_some() {
                    return Err(recording_conflict("มีการประชุมที่กำลังอัดอยู่"));
                }
                let owner_key = storage::random_id()?;
                let meeting = storage::start(root, read_json(&mut request)?)?;
                let response = serde_json::json!({"meeting": meeting, "recording_key": owner_key});
                *active = Some(ActiveRecording {
                    id: meeting.id,
                    owner_key,
                    last_seen: Instant::now(),
                });
                Ok(response)
            })();
            json(request, result);
        }
        (&Method::Post, "/api/quit") => {
            if active.is_some() {
                json(
                    request,
                    Err::<(), _>(recording_conflict("หยุดบันทึกในหน้าต่างที่กำลังอัดก่อนออกจากแอป")),
                );
                return true;
            }
            json(request, Ok(()));
            return false;
        }
        _ => {
            let route = path
                .strip_prefix("/api/")
                .and_then(|tail| tail.split_once('/'))
                .filter(|(id, _)| storage::valid_id(id));
            match (method, route) {
                (Method::Get, Some((id, "audio"))) => {
                    let root = root.to_owned();
                    let id = id.to_owned();
                    std::thread::spawn(move || send_audio(request, &root, &id));
                }
                (Method::Post, Some((id, "audio"))) => {
                    let result = recording_access(active, &request, id).and_then(|()| {
                        request_header(&request, "X-Frame-Offset")
                            .and_then(|value| value.parse().ok())
                            .ok_or(io::ErrorKind::InvalidInput.into())
                            .and_then(|offset| {
                                storage::append(
                                    root,
                                    id,
                                    offset,
                                    &read_bytes(&mut request, 524_288)?,
                                )
                            })
                    });
                    json(request, result);
                }
                (Method::Post, Some((id, "speaker"))) => {
                    let result = recording_access(active, &request, id)
                        .and_then(|()| storage::change(root, id, read_json(&mut request)?));
                    json(request, result);
                }
                (Method::Post, Some((id, "mark"))) => {
                    let result = recording_access(active, &request, id)
                        .and_then(|()| storage::mark(root, id, read_json(&mut request)?));
                    json(request, result);
                }
                (Method::Post, Some((id, "heartbeat"))) => {
                    let result = recording_access(active, &request, id);
                    json(request, result);
                }
                (Method::Post, Some((id, "abandon"))) => {
                    let result = recording_access(active, &request, id).map(|()| *active = None);
                    json(request, result);
                }
                (Method::Post, Some((id, "finish"))) => {
                    let result = recording_access(active, &request, id)
                        .and_then(|()| storage::finish(root, id))
                        .inspect(|_| *active = None);
                    json(request, result);
                }
                (Method::Post, Some((id, "recover"))) => {
                    let result = if active.is_some() {
                        Err(recording_conflict("หยุดบันทึกในหน้าต่างที่กำลังอัดก่อนกู้ไฟล์"))
                    } else {
                        storage::finish(root, id)
                    };
                    json(request, result);
                }
                _ => json(request, Err::<(), _>(io::ErrorKind::NotFound.into())),
            }
        }
    }
    true
}

fn launch_edge(url: &str) -> io::Result<()> {
    let exe = ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(env::var_os)
        .map(|path| PathBuf::from(path).join("Microsoft/Edge/Application/msedge.exe"))
        .find(|path| path.is_file())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Microsoft Edge not found"))?;
    Command::new(exe).arg(format!("--app={url}")).spawn()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let open_window = !env::args().any(|arg| arg == "--no-open");
    let root = env::var_os("MEETING_NOTES_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("LOCALAPPDATA")
                .map_or_else(env::temp_dir, PathBuf::from)
                .join("Meeting Notes")
                .join("meetings")
        });
    fs::create_dir_all(&root)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".lock"))?;
    let address_file = root.join(".server-address");
    if let Err(error) = lock.try_lock() {
        if open_window
            && let Ok(value) = fs::read_to_string(&address_file)
            && let Ok(address) = value.parse::<SocketAddr>()
            && address.ip().is_loopback()
            && TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
        {
            launch_edge(&format!("http://{address}"))?;
            return Ok(());
        }
        return Err(io::Error::other(format!(
            "Meeting Notes is already using this folder: {error}"
        ))
        .into());
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let host = listener.local_addr()?.to_string();
    let url = format!("http://{host}");
    let key = storage::random_id()?;
    let server = Server::from_listener(listener, None)?;
    fs::write(&address_file, &host)?;
    println!("Meeting Notes: {url}");
    if open_window && let Err(error) = launch_edge(&url) {
        eprintln!("Open the address above in a browser: {error}");
    }
    let mut active = None;
    for request in server.incoming_requests() {
        if !handle(request, &root, &host, &key, &mut active) {
            break;
        }
    }
    Ok(())
}
