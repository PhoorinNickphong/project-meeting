use serde_json::{Value, json};
use std::{
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("meeting-windows-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    address: SocketAddr,
}

impl Server {
    fn launch(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_meeting-notes"))
            .arg("--no-open")
            .env("MEETING_NOTES_DATA_DIR", root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .trim()
            .strip_prefix("Meeting Notes: http://")
            .expect("server announced loopback address")
            .parse()
            .unwrap();
        Self { child, address }
    }

    fn window(&self) -> Window {
        let mut window = Window {
            address: self.address,
            app_key: String::new(),
        };
        let (status, page) = window.request("GET", "/", &[], b"");
        assert_eq!(status, 200);
        window.app_key = page
            .split_once(r#"name="meeting-key" content=""#)
            .unwrap()
            .1
            .split_once('"')
            .unwrap()
            .0
            .to_owned();
        assert_eq!(window.app_key.len(), 32);
        window
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Window {
    address: SocketAddr,
    app_key: String,
}

impl Window {
    fn request(
        &self,
        method: &str,
        route: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, String) {
        let timeout = Duration::from_secs(5);
        let mut socket = TcpStream::connect_timeout(&self.address, timeout).unwrap();
        socket.set_read_timeout(Some(timeout)).unwrap();
        write!(
            socket,
            "{method} {route} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nX-Meeting-Key: {}\r\nContent-Length: {}\r\n",
            self.address,
            self.app_key,
            body.len(),
        )
        .unwrap();
        for (name, value) in headers {
            write!(socket, "{name}: {value}\r\n").unwrap();
        }
        socket.write_all(b"\r\n").unwrap();
        socket.write_all(body).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, body.into())
    }

    fn start(&self) -> (String, String) {
        let body = json!({"title":"ประชุมทดสอบ","speakers":["คุณเอ"],"sample_rate":16000});
        let (status, response) =
            self.request("POST", "/api/start", &[], body.to_string().as_bytes());
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&response).unwrap();
        (
            value["meeting"]["id"].as_str().unwrap().to_owned(),
            value["recording_key"].as_str().unwrap().to_owned(),
        )
    }

    fn list(&self) -> Value {
        let (status, response) = self.request("GET", "/api/meetings", &[], b"");
        assert_eq!(status, 200);
        serde_json::from_str(&response).unwrap()
    }

    fn append(&self, id: &str, key: &str, offset: u64) {
        let (status, body) = self.request(
            "POST",
            &format!("/api/{id}/audio"),
            &[
                ("X-Recording-Key", key),
                ("X-Frame-Offset", &offset.to_string()),
            ],
            &[8; 3200],
        );
        assert_eq!(status, 200);
        assert_eq!(body.parse::<u64>().unwrap(), offset + 1600);
    }
}

#[test]
fn a_second_window_cannot_finalize_or_exit_a_live_recording() {
    let dir = TestDirectory::new();
    let server = Server::launch(&dir.0);
    let owner = server.window();
    let other = server.window();
    let (interrupted, closed_key) = owner.start();
    owner.append(&interrupted, &closed_key, 0);
    assert_eq!(
        owner
            .request(
                "POST",
                &format!("/api/{interrupted}/abandon"),
                &[("X-Recording-Key", &closed_key)],
                b""
            )
            .0,
        200
    );
    let (id, key) = owner.start();
    owner.append(&id, &key, 0);
    let meetings = other.list();
    let listed = meetings
        .as_array()
        .unwrap()
        .iter()
        .find(|meeting| meeting["id"] == id)
        .unwrap();
    assert_eq!(listed["recording"], true);
    assert!(listed.get("recording_key").is_none());
    for route in [
        format!("/api/{id}/recover"),
        format!("/api/{interrupted}/recover"),
        format!("/api/{id}/finish"),
        "/api/quit".into(),
    ] {
        assert_eq!(other.request("POST", &route, &[], b"").0, 409, "{route}");
    }
    owner.append(&id, &key, 1600);
    let (status, body) = owner.request(
        "POST",
        &format!("/api/{id}/finish"),
        &[("X-Recording-Key", &key)],
        b"",
    );
    assert_eq!(status, 200);
    let meeting: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(meeting["frames"], 3200);
    assert_eq!(meeting["finished"], true);
    assert!(
        other
            .list()
            .as_array()
            .unwrap()
            .iter()
            .all(|meeting| meeting["recording"] == false)
    );
    assert_eq!(
        other
            .request("POST", &format!("/api/{interrupted}/recover"), &[], b"")
            .0,
        200
    );
    assert_eq!(other.request("POST", "/api/quit", &[], b"").0, 200);
}

#[test]
fn a_recording_is_recoverable_after_an_abrupt_server_exit() {
    let dir = TestDirectory::new();
    let server = Server::launch(&dir.0);
    let owner = server.window();
    let (id, key) = owner.start();
    owner.append(&id, &key, 0);
    drop(server);

    let restarted = Server::launch(&dir.0);
    let other = restarted.window();
    assert_eq!(other.list()[0]["recording"], false);
    let (status, body) = other.request("POST", &format!("/api/{id}/recover"), &[], b"");
    assert_eq!(status, 200);
    let meeting: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(meeting["finished"], true);
    assert_eq!(meeting["frames"], 1600);
    assert_eq!(
        fs::metadata(dir.0.join(id).join("audio.wav"))
            .unwrap()
            .len(),
        3244
    );
}
