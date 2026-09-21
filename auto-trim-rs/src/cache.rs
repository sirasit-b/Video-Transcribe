//! Caching the audio levels.
//!
//! Analysis is deterministic: the same file at the same timebase always produces
//! the same levels, whatever threshold or margin is asked for afterwards. Tuning a
//! threshold therefore costs nothing after the first pass — which matters most on a
//! long video, where the analysis that would be repeated takes a while (twelve
//! seconds for three hours, and that is with fast audio; a dense stereo track is
//! several times that). auto-editor caches the same thing for the same reason.
//!
//! The cache is disposable: a miss, a corrupt file or a full disk costs a re-analysis
//! and nothing else.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::audio::StreamAnalysis;
use crate::probe::Rational;

const MAGIC: &[u8; 4] = b"ATL1";

/// Everything that changes the levels. A file edited in place gets a new key
/// through its size and modification time.
pub struct Key {
    pub source: PathBuf,
    pub timebase: Rational,
    pub ordinal: usize,
    pub sample_rate: u32,
    pub channels: usize,
}

/// FNV-1a: small, and stable across builds in a way that a hasher from the standard
/// library is not promised to be.
fn hash(bytes: &[u8]) -> u64 {
    let mut value: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        value ^= *byte as u64;
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    value
}

impl Key {
    fn file_name(&self) -> Option<String> {
        let metadata = fs::metadata(&self.source).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let identity = format!(
            "{}|{}|{}|{}|{}/{}|{}|{}|{}",
            self.source.display(),
            metadata.len(),
            modified.as_secs(),
            modified.subsec_nanos(),
            self.timebase.num,
            self.timebase.den,
            self.ordinal,
            self.sample_rate,
            self.channels
        );
        Some(format!("{:016x}.levels", hash(identity.as_bytes())))
    }
}

pub struct Cache {
    directory: Option<PathBuf>,
    limit_bytes: u64,
}

impl Cache {
    /// `None` for the directory turns the cache off.
    pub fn new(directory: Option<PathBuf>, limit_bytes: u64) -> Cache {
        if let Some(path) = &directory {
            if let Err(err) = fs::create_dir_all(path) {
                eprintln!("auto-trim: level cache disabled ({}): {err}", path.display());
                return Cache {
                    directory: None,
                    limit_bytes,
                };
            }
        }
        Cache {
            directory,
            limit_bytes,
        }
    }

    fn path_for(&self, key: &Key) -> Option<PathBuf> {
        let directory = self.directory.as_ref()?;
        Some(directory.join(key.file_name()?))
    }

    pub fn load(&self, key: &Key) -> Option<StreamAnalysis> {
        let path = self.path_for(key)?;
        let mut file = fs::File::open(&path).ok()?;
        let mut header = [0u8; 12];
        file.read_exact(&mut header).ok()?;
        if &header[..4] != MAGIC {
            return None;
        }
        let count = u64::from_le_bytes(header[4..12].try_into().ok()?) as usize;
        // A frame is 2 bytes of level plus 8 of boundary, and there is one more
        // boundary than there are frames.
        let mut body = Vec::new();
        file.read_to_end(&mut body).ok()?;
        if body.len() != count * 2 + (count + 1) * 8 {
            return None;
        }

        let mut levels = Vec::with_capacity(count);
        for index in 0..count {
            let at = index * 2;
            levels.push(u16::from_le_bytes([body[at], body[at + 1]]));
        }
        let mut boundaries = Vec::with_capacity(count + 1);
        let base = count * 2;
        for index in 0..=count {
            let at = base + index * 8;
            boundaries.push(u64::from_le_bytes(body[at..at + 8].try_into().ok()?));
        }

        // Touching the file is what keeps the eviction order meaningful.
        let _ = fs::File::open(&path).and_then(|file| file.set_modified(std::time::SystemTime::now()));
        Some(StreamAnalysis { levels, boundaries })
    }

    pub fn store(&self, key: &Key, analysis: &StreamAnalysis) {
        let Some(path) = self.path_for(key) else {
            return;
        };
        if analysis.boundaries.len() != analysis.levels.len() + 1 {
            return;
        }

        let mut bytes =
            Vec::with_capacity(12 + analysis.levels.len() * 2 + analysis.boundaries.len() * 8);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(analysis.levels.len() as u64).to_le_bytes());
        for level in &analysis.levels {
            bytes.extend_from_slice(&level.to_le_bytes());
        }
        for boundary in &analysis.boundaries {
            bytes.extend_from_slice(&boundary.to_le_bytes());
        }

        // Write beside the target and rename, so a reader never sees half a file.
        let temporary = path.with_extension("partial");
        if fs::File::create(&temporary)
            .and_then(|mut file| file.write_all(&bytes))
            .and_then(|_| fs::rename(&temporary, &path))
            .is_err()
        {
            let _ = fs::remove_file(&temporary);
            return;
        }
        self.evict();
    }

    /// Drop the least recently used entries once the directory outgrows its limit.
    fn evict(&self) {
        let Some(directory) = &self.directory else {
            return;
        };
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };

        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                if !metadata.is_file() {
                    return None;
                }
                Some((
                    metadata.modified().ok()?,
                    metadata.len(),
                    entry.path(),
                ))
            })
            .collect();

        let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
        if total <= self.limit_bytes {
            return;
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in files {
            if total <= self.limit_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis() -> StreamAnalysis {
        StreamAnalysis {
            levels: vec![0, 2621, 65535, 7],
            boundaries: vec![0, 1600, 3200, 4800, 6000],
        }
    }

    #[test]
    fn a_stored_analysis_comes_back_unchanged() {
        let directory = std::env::temp_dir().join(format!("auto-trim-cache-{}", std::process::id()));
        let cache = Cache::new(Some(directory.clone()), 1 << 30);
        let source = directory.join("source.mp4");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&source, b"not really a video").unwrap();

        let key = Key {
            source: source.clone(),
            timebase: Rational { num: 30, den: 1 },
            ordinal: 0,
            sample_rate: 48000,
            channels: 2,
        };

        assert!(cache.load(&key).is_none(), "nothing stored yet");
        cache.store(&key, &analysis());
        let loaded = cache.load(&key).expect("stored analysis");
        assert_eq!(loaded.levels, analysis().levels);
        assert_eq!(loaded.boundaries, analysis().boundaries);

        // A different timebase is a different analysis.
        let other = Key {
            timebase: Rational { num: 25, den: 1 },
            ..key
        };
        assert!(cache.load(&other).is_none());

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn editing_the_source_invalidates_the_entry() {
        let directory = std::env::temp_dir().join(format!("auto-trim-edit-{}", std::process::id()));
        let cache = Cache::new(Some(directory.clone()), 1 << 30);
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source.mp4");
        fs::write(&source, b"first").unwrap();

        let key = || Key {
            source: source.clone(),
            timebase: Rational { num: 30, den: 1 },
            ordinal: 0,
            sample_rate: 48000,
            channels: 2,
        };
        cache.store(&key(), &analysis());
        assert!(cache.load(&key()).is_some());

        // Same path, different contents: the size alone moves the key.
        fs::write(&source, b"second and longer").unwrap();
        assert!(cache.load(&key()).is_none());

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_disabled_cache_never_stores() {
        let cache = Cache::new(None, 0);
        let key = Key {
            source: PathBuf::from("/nowhere.mp4"),
            timebase: Rational { num: 30, den: 1 },
            ordinal: 0,
            sample_rate: 48000,
            channels: 1,
        };
        cache.store(&key, &analysis());
        assert!(cache.load(&key).is_none());
    }
}
