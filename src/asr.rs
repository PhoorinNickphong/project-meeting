mod audio;
mod model;

use crate::storage;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MODEL_FOLDER: &str = "models/OpenThai2.0-ASR-Streaming-ONNX";
const MODEL_FILES: [(&str, u64); 4] = [
    ("encoder.onnx", 41_941_207),
    ("encoder.onnx.data", 2_454_405_120),
    ("decoder_joint.onnx", 107_330_778),
    ("tokens.txt", 93_160),
];

#[derive(Serialize)]
pub struct Config {
    folder: PathBuf,
    ready: bool,
    message: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Segment {
    start_frame: u64,
    end_frame: u64,
    speaker: Option<u32>,
    text: String,
}

#[derive(Deserialize, Serialize)]
pub struct Transcript {
    model: String,
    sample_rate: u32,
    segments: Vec<Segment>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Idle,
    Running { processed_frames: u64 },
    Complete { transcript: Transcript },
    Failed { message: String },
}

#[derive(Default)]
struct Jobs {
    running: Option<(String, u64)>,
    errors: HashMap<String, String>,
}

#[derive(Default)]
pub struct Asr(Arc<Mutex<Jobs>>);

impl Asr {
    pub fn busy(&self) -> bool {
        self.0.lock().unwrap().running.is_some()
    }

    pub fn status(&self, root: &Path, id: &str) -> io::Result<Status> {
        storage::load(root, id)?;
        {
            let jobs = self.0.lock().unwrap();
            if let Some((running, processed_frames)) = &jobs.running
                && running == id
            {
                return Ok(Status::Running {
                    processed_frames: *processed_frames,
                });
            }
            if let Some(message) = jobs.errors.get(id) {
                return Ok(Status::Failed {
                    message: message.clone(),
                });
            }
        }
        match fs::File::open(root.join(id).join("transcript.json")) {
            Ok(file) => Ok(Status::Complete {
                transcript: serde_json::from_reader(file)?,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Status::Idle),
            Err(error) => Err(error),
        }
    }

    pub fn start(&self, root: &Path, id: &str) -> io::Result<Status> {
        let meeting = storage::load(root, id)?;
        if !meeting.finished || meeting.frames == 0 {
            return Err(invalid("ต้องหยุดบันทึกหรือกู้ไฟล์ WAV ที่มีเสียงก่อนถอดข้อความ"));
        }
        let mut jobs = self.0.lock().unwrap();
        if let Some((running, processed_frames)) = &jobs.running {
            if running == id {
                return Ok(Status::Running {
                    processed_frames: *processed_frames,
                });
            }
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "กำลังถอดเสียงการประชุมอื่น กรุณารอให้เสร็จ",
            ));
        }
        let model = validate_folder(&model_folder(root))?;
        let dll = runtime()?;
        let directory = root.join(id);
        let jobs_ref = Arc::clone(&self.0);
        jobs.running = Some((id.to_owned(), 0));
        jobs.errors.remove(id);
        if let Err(error) = std::thread::Builder::new()
            .name("openthai".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(|| {
                    transcribe(&directory, &meeting, &model, &dll, |cursor| {
                        if let Some((_, progress)) = &mut jobs_ref.lock().unwrap().running {
                            *progress = cursor;
                        }
                    })
                })
                .unwrap_or_else(|_| Err(io::Error::other("OpenThai หยุดทำงานระหว่างถอดเสียง")));
                let mut jobs = jobs_ref.lock().unwrap();
                jobs.running = None;
                if let Err(error) = outcome {
                    eprintln!("OpenThai {}: {error}", meeting.id);
                    jobs.errors.insert(
                        meeting.id,
                        format!("ถอดเสียงไม่สำเร็จ กรุณาตรวจไฟล์โมเดลและพื้นที่ดิสก์แล้วลองอีกครั้ง ({error})"),
                    );
                }
            })
        {
            jobs.running = None;
            return Err(error);
        }
        Ok(Status::Running {
            processed_frames: 0,
        })
    }
}

pub fn config(root: &Path) -> Config {
    let folder = model_folder(root);
    let status = validate_folder(&folder).and_then(|_| runtime());
    Config {
        folder,
        ready: status.is_ok(),
        message: status.err().map_or_else(
            || "ไฟล์ OpenThai FP32 และ ONNX Runtime ครบ".into(),
            |error| error.to_string(),
        ),
    }
}

pub fn set_folder(root: &Path, folder: &Path) -> io::Result<Config> {
    let path = validate_folder(folder)?;
    let path = path
        .to_str()
        .ok_or_else(|| invalid("อ่านชื่อโฟลเดอร์โมเดลไม่ได้"))?;
    write_file(root, "asr-model-path.txt", |file| {
        file.write_all(path.as_bytes())
    })?;
    Ok(config(root))
}

fn model_folder(root: &Path) -> PathBuf {
    let saved = fs::read_to_string(root.join("asr-model-path.txt"))
        .ok()
        .map(PathBuf::from);
    let exe = env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    // Cargo builds live in <project>/target/<profile>. Portable builds keep models beside the EXE.
    let project = exe_dir
        .and_then(Path::parent)
        .filter(|dir| dir.file_name().is_some_and(|name| name == "target"))
        .and_then(Path::parent);
    let candidates = [
        saved,
        env::var_os("MEETING_NOTES_ASR_MODEL_DIR").map(PathBuf::from),
        exe_dir.map(|dir| dir.join(MODEL_FOLDER)),
        project.map(|dir| dir.join(MODEL_FOLDER)),
        env::current_dir().ok().map(|dir| dir.join(MODEL_FOLDER)),
    ];
    candidates
        .iter()
        .flatten()
        .find(|path| validate_folder(path).is_ok())
        .or_else(|| candidates.iter().flatten().next())
        .cloned()
        .unwrap_or_else(|| PathBuf::from(MODEL_FOLDER))
}

fn validate_folder(folder: &Path) -> io::Result<PathBuf> {
    let local = |path: &Path| {
        #[cfg(windows)]
        return path.is_absolute()
            && matches!(path.components().next(),
            Some(std::path::Component::Prefix(prefix)) if matches!(prefix.kind(),
                std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)));
        #[cfg(not(windows))]
        path.is_absolute()
    };
    if !local(folder) {
        return Err(invalid("ระบุพาธโฟลเดอร์โมเดลแบบเต็มบนไดรฟ์ในเครื่อง"));
    }
    let folder = folder
        .canonicalize()
        .map_err(|_| invalid("ไม่พบโฟลเดอร์ OpenThai FP32 กรุณาระบุในหน้าตั้งค่า"))?;
    if !local(&folder) {
        return Err(invalid("เลือกโฟลเดอร์ OpenThai จากไดรฟ์ในเครื่อง"));
    }
    for (name, size) in MODEL_FILES {
        let path = folder.join(name);
        let valid = path.canonicalize().ok().is_some_and(|file| file == path)
            && fs::metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() == size);
        if !valid {
            return Err(invalid(format!(
                "ไฟล์ {name} หายหรือขนาดไม่ตรงกับ OpenThai FP32 กรุณาตรวจไฟล์ที่ดาวน์โหลด"
            )));
        }
    }
    Ok(folder)
}

fn runtime() -> io::Result<PathBuf> {
    let dll = env::current_exe()?.with_file_name("onnxruntime.dll");
    if dll.is_file() {
        return Ok(dll);
    }
    Err(invalid(
        "ไม่พบ onnxruntime.dll ให้วาง ONNX Runtime CPU รุ่น 1.24.4 ข้างไฟล์ meeting-notes.exe",
    ))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn transcribe(
    directory: &Path,
    meeting: &storage::Meeting,
    model_path: &Path,
    dll: &Path,
    progress: impl FnMut(u64),
) -> io::Result<()> {
    let mut model = None;
    let mut segments = Vec::new();
    audio::read_segments(
        &directory.join("audio.wav"),
        meeting,
        |start_frame, end_frame, speaker, samples| {
            let model = match &mut model {
                Some(model) => model,
                slot => slot.insert(model::OpenThai::open(model_path, dll)?),
            };
            let text = model.decode(samples, meeting.sample_rate as usize)?;
            if !text.is_empty() {
                segments.push(Segment {
                    start_frame,
                    end_frame,
                    speaker,
                    text,
                });
            }
            Ok(())
        },
        progress,
    )?;
    let transcript = Transcript {
        model: "iapp/OpenThai2.0-ASR-Streaming-ONNX FP32".into(),
        sample_rate: meeting.sample_rate,
        segments,
    };
    write_file(directory, "transcript.json", |file| {
        Ok(serde_json::to_writer(file, &transcript)?)
    })
}

fn write_file(
    directory: &Path,
    name: &str,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let pending = directory.join(format!("{name}.pending"));
    let mut file = fs::File::create(&pending)?;
    write(&mut file)?;
    file.sync_all()?;
    drop(file);
    fs::rename(pending, directory.join(name))
}
