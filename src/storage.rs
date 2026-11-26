use meeting_notes_core::{Speaker, SpeakerTimeline};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    num::NonZeroU32,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const WAV_HEADER_SIZE: u64 = 44;
const MAX_PCM_BYTES: u64 = u32::MAX as u64 - 37;

#[derive(Deserialize)]
pub struct Setup {
    title: String,
    speakers: Vec<String>,
    sample_rate: u32,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Change {
    pub frame: u64,
    pub speaker: Option<u32>,
}

#[derive(Deserialize, Serialize)]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub speakers: Vec<String>,
    pub sample_rate: u32,
    pub created_at: u64,
    pub frames: u64,
    pub finished: bool,
    pub changes: Vec<Change>,
    pub markers: Vec<u64>,
}

pub fn random_id() -> io::Result<String> {
    use std::fmt::Write as _;
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(id, "{byte:02x}").expect("writing to a String is infallible");
    }
    Ok(id)
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn load(root: &Path, id: &str) -> io::Result<Meeting> {
    if !valid_id(id) {
        return Err(io::ErrorKind::NotFound.into());
    }
    let dir = root.join(id);
    let mut meeting: Meeting = serde_json::from_reader(File::open(dir.join("meeting.json"))?)?;
    meeting.frames = (fs::metadata(dir.join("audio.wav"))?
        .len()
        .saturating_sub(WAV_HEADER_SIZE))
        / 2;
    Ok(meeting)
}

fn save(root: &Path, meeting: &Meeting) -> io::Result<()> {
    let dir = root.join(&meeting.id);
    let temporary = dir.join("meeting.pending");
    let mut file = File::create(&temporary)?;
    serde_json::to_writer(&mut file, meeting)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, dir.join("meeting.json"))
}

fn wav_header(output: &mut File, rate: u32, size: u32) -> io::Result<()> {
    output.seek(SeekFrom::Start(0))?;
    let mut header = [0; WAV_HEADER_SIZE as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(size + 36).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&1_u16.to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&(rate * 2).to_le_bytes());
    header[32..34].copy_from_slice(&2_u16.to_le_bytes());
    header[34..36].copy_from_slice(&16_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&size.to_le_bytes());
    output.write_all(&header)
}

pub fn start(root: &Path, setup: Setup) -> io::Result<Meeting> {
    let title = setup.title.trim();
    let speakers: Vec<_> = setup.speakers.iter().map(|name| name.trim()).collect();
    if title.is_empty()
        || title.chars().count() > 100
        || !(1..=5).contains(&speakers.len())
        || speakers
            .iter()
            .any(|name| name.is_empty() || name.chars().count() > 60)
        || !(8_000..=192_000).contains(&setup.sample_rate)
    {
        return Err(invalid("ชื่อหรือการตั้งค่าการประชุมไม่ถูกต้อง"));
    }
    let meeting = Meeting {
        id: random_id()?,
        title: title.into(),
        speakers: speakers.into_iter().map(str::to_owned).collect(),
        sample_rate: setup.sample_rate,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs(),
        frames: 0,
        finished: false,
        changes: Vec::new(),
        markers: Vec::new(),
    };
    let dir = root.join(&meeting.id);
    fs::create_dir(&dir)?;
    let mut file = File::create(dir.join("audio.wav"))?;
    wav_header(&mut file, meeting.sample_rate, 0)?;
    save(root, &meeting)?;
    Ok(meeting)
}

pub fn list(root: &Path) -> io::Result<Vec<Meeting>> {
    let mut meetings = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if let Some(id) = entry.file_name().to_str().filter(|id| valid_id(id))
            && let Ok(meeting) = load(root, id)
        {
            meetings.push(meeting);
        }
    }
    meetings.sort_by_key(|meeting| std::cmp::Reverse(meeting.created_at));
    Ok(meetings)
}

pub fn append(root: &Path, id: &str, offset: u64, pcm: &[u8]) -> io::Result<u64> {
    let meeting = load(root, id)?;
    if meeting.finished || pcm.is_empty() || !pcm.len().is_multiple_of(2) {
        return Err(invalid("เขียนข้อมูลเสียงช่วงนี้ไม่ได้"));
    }
    let path = root.join(id).join("audio.wav");
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let end_frame = offset
        .checked_add(pcm.len() as u64 / 2)
        .filter(|end| *end <= MAX_PCM_BYTES / 2)
        .ok_or_else(|| invalid("ไฟล์เสียงเต็มขนาด WAV"))?;
    if offset < meeting.frames && end_frame <= meeting.frames {
        // The client may retry after the server wrote the chunk but lost its response.
        use std::io::Read;
        let mut recorded = vec![0; pcm.len()];
        file.seek(SeekFrom::Start(WAV_HEADER_SIZE + offset * 2))?;
        file.read_exact(&mut recorded)?;
        if recorded == pcm {
            return Ok(end_frame);
        }
    }
    if offset != meeting.frames {
        return Err(invalid("ตำแหน่งข้อมูลเสียงไม่ต่อเนื่อง"));
    }
    let previous_length = WAV_HEADER_SIZE + offset * 2;
    file.seek(SeekFrom::Start(previous_length))?;
    if let Err(error) = file.write_all(pcm) {
        let _ = file.set_len(previous_length);
        return Err(error);
    }
    wav_header(&mut file, meeting.sample_rate, (end_frame * 2) as u32)?;
    Ok(end_frame)
}

fn speaker(value: Option<u32>) -> Speaker {
    value
        .and_then(NonZeroU32::new)
        .map_or(Speaker::Unidentified, Speaker::Participant)
}

pub fn change(root: &Path, id: &str, request: Change) -> io::Result<()> {
    let mut meeting = load(root, id)?;
    if meeting.finished
        || request.frame > meeting.frames
        || request
            .speaker
            .is_some_and(|id| id == 0 || id as usize > meeting.speakers.len())
    {
        return Err(invalid("ตำแหน่งหรือผู้พูดไม่ถูกต้อง"));
    }
    let mut timeline = SpeakerTimeline::default();
    for change in &meeting.changes {
        timeline
            .record_change(change.frame, speaker(change.speaker))
            .map_err(|_| invalid("ลำดับผู้พูดในไฟล์ไม่ถูกต้อง"))?;
    }
    timeline
        .record_change(request.frame, speaker(request.speaker))
        .map_err(|_| invalid("ตำแหน่งเปลี่ยนผู้พูดย้อนเวลา"))?;
    meeting.changes = timeline
        .changes()
        .iter()
        .map(|change| Change {
            frame: change.audio_frame,
            speaker: match change.speaker {
                Speaker::Unidentified => None,
                Speaker::Participant(id) => Some(id.get()),
            },
        })
        .collect();
    save(root, &meeting)
}

pub fn mark(root: &Path, id: &str, frame: u64) -> io::Result<()> {
    let mut meeting = load(root, id)?;
    if meeting.finished || frame > meeting.frames {
        return Err(invalid("ตำแหน่งช่วงสำคัญไม่ถูกต้อง"));
    }
    match meeting.markers.binary_search(&frame) {
        Ok(_) => Ok(()),
        Err(index) => {
            meeting.markers.insert(index, frame);
            save(root, &meeting)
        }
    }
}

pub fn finish(root: &Path, id: &str) -> io::Result<Meeting> {
    let mut meeting = load(root, id)?;
    let mut file = OpenOptions::new()
        .write(true)
        .open(root.join(id).join("audio.wav"))?;
    let bytes = meeting.frames * 2;
    if bytes > MAX_PCM_BYTES {
        return Err(invalid("ไฟล์เสียงเกินขนาด WAV"));
    }
    file.set_len(WAV_HEADER_SIZE + bytes)?;
    wav_header(&mut file, meeting.sample_rate, bytes as u32)?;
    file.sync_all()?;
    meeting.finished = true;
    save(root, &meeting)?;
    Ok(meeting)
}

pub fn audio(root: &Path, id: &str) -> io::Result<File> {
    if !load(root, id)?.finished {
        return Err(invalid("กรุณาจบหรือกู้การประชุมก่อนเปิดเสียง"));
    }
    File::open(root.join(id).join("audio.wav"))
}
