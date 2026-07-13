use std::fs::File;
use std::io::{self, BufRead};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct KeyManager {
    keys: Vec<String>,
    current_index: AtomicUsize,
}

impl KeyManager {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let file = File::open(path)?;
        let reader = io::BufReader::new(file);
        let mut keys = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                keys.push(trimmed.to_string());
            }
        }

        if keys.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No keys found in file",
            ));
        }

        Ok(Self {
            keys,
            current_index: AtomicUsize::new(0),
        })
    }

    pub fn get_next_key(&self) -> String {
        let index = self.current_index.fetch_add(1, Ordering::Relaxed);
        self.keys[index % self.keys.len()].clone()
    }
}
