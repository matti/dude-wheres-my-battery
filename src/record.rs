use crate::types::Sample;
use flate2::{Compression, read::MultiGzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Session {
        schema: u32,
        timestamp: f64,
        interval_s: f64,
        chip: String,
        model: String,
        recorder_pid: u32,
    },
    Sample {
        timestamp: f64,
        elapsed_s: f64,
        gap: bool,
        sampler_ms: f64,
        sample: Box<Sample>,
    },
    Marker {
        timestamp: f64,
        elapsed_s: f64,
        label: String,
    },
    Context {
        timestamp: f64,
        elapsed_s: f64,
        assertions: String,
    },
    Notice {
        timestamp: f64,
        message: String,
    },
    Vm {
        elapsed_s: f64,
        inspection: Box<crate::vm::Inspection>,
    },
    End {
        timestamp: f64,
    },
}

pub struct Recorder {
    file: File,
    pub path: PathBuf,
}
impl Recorder {
    pub fn new(dir: &Path, interval_s: f64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!(
            "session-{}-{}.jsonl.gz",
            (now() * 1000.) as u64,
            std::process::id()
        ));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut result = Self { file, path };
        result.write(&Record::Session {
            schema: 1,
            timestamp: now(),
            interval_s,
            chip: crate::host::chip(),
            model: crate::host::model(),
            recorder_pid: std::process::id(),
        })?;
        Ok(result)
    }
    pub fn write(&mut self, record: &Record) -> io::Result<()> {
        let mut data = serde_json::to_vec(record)?;
        data.push(b'\n');
        // Independent gzip members make completed events readable during capture
        // and recoverable even if the final member is interrupted.
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        gzip.write_all(&data)?;
        self.file.write_all(&gzip.finish()?)?;
        // Commit before drawing. Errors stop recording visibly, never silently.
        self.file.sync_data()
    }
}

pub fn read(path: &Path, mut consume: impl FnMut(Record)) -> io::Result<usize> {
    let mut file = BufReader::new(File::open(path)?);
    let compressed = file.fill_buf()?.starts_with(&[0x1f, 0x8b]);
    let input: Box<dyn Read> = if compressed {
        Box::new(MultiGzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut reader = BufReader::new(input);
    let mut line = String::new();
    let mut count = 0;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if compressed && e.kind() == io::ErrorKind::UnexpectedEof => {
                eprintln!(
                    "Ignoring incomplete trailing gzip member in {}",
                    path.display()
                );
                break;
            }
            Err(e) => return Err(e),
        }
        // An interrupted writer can leave one partial trailing record.
        if !line.ends_with('\n') {
            eprintln!("Ignoring incomplete trailing record in {}", path.display());
            break;
        }
        let record: Record = serde_json::from_str(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("record {}: {e}", count + 1),
            )
        })?;
        if let Record::Session { schema, .. } = record
            && schema != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unsupported recording schema",
            ));
        }
        consume(record);
        count += 1;
    }
    Ok(count)
}

pub fn latest(dir: &Path) -> io::Result<PathBuf> {
    std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_recording(p))
        .max()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No recordings found"))
}

pub fn is_recording(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.starts_with("session-") && (n.ends_with(".jsonl") || n.ends_with(".jsonl.gz"))
    })
}
