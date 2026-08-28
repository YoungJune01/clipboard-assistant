use std::{
    collections::HashSet,
    io::Cursor,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

use image::{DynamicImage, ImageFormat};

use crate::{
    domain::{ClipboardRecord, ClipboardRepresentation, ContentIdentity, RecordId},
    services::persistence::PersistenceWorker,
};

const QUEUE_CAPACITY: usize = 8;
const MAX_OCR_BYTES: usize = 64 * 1024;
const MAX_QR_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecognitionOptions {
    pub ocr: bool,
    pub qr: bool,
}

#[derive(Clone, Debug)]
struct RecognitionJob {
    id: RecordId,
    identity: ContentIdentity,
    image: Vec<u8>,
    options: RecognitionOptions,
}

pub(crate) struct RecognitionService {
    sender: Mutex<Option<mpsc::SyncSender<RecognitionJob>>>,
    scheduled: Arc<Mutex<HashSet<ContentIdentity>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    options: Mutex<RecognitionOptions>,
}

impl RecognitionService {
    pub(crate) fn start(
        persistence: Arc<PersistenceWorker>,
        on_saved: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Arc<Self>> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let scheduled = Arc::new(Mutex::new(HashSet::new()));
        let worker_scheduled = Arc::clone(&scheduled);
        let thread = thread::Builder::new()
            .name("clipboard-recognition".to_owned())
            .spawn(move || run_worker(receiver, persistence, worker_scheduled, on_saved))?;
        Ok(Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            scheduled,
            thread: Mutex::new(Some(thread)),
            options: Mutex::new(RecognitionOptions::default()),
        }))
    }

    pub(crate) fn set_options(&self, options: RecognitionOptions) {
        *lock_unpoisoned(&self.options) = options;
    }

    pub(crate) fn enqueue(&self, record: &ClipboardRecord) -> bool {
        let options = *lock_unpoisoned(&self.options);
        if (!options.ocr && !options.qr) || record.sensitive {
            return false;
        }
        let Some(image) = image_bytes(record) else {
            return false;
        };
        let mut scheduled = lock_unpoisoned(&self.scheduled);
        if !scheduled.insert(record.content_identity.clone()) {
            return false;
        }
        let job = RecognitionJob {
            id: record.id,
            identity: record.content_identity.clone(),
            image,
            options,
        };
        let sent = lock_unpoisoned(&self.sender)
            .as_ref()
            .is_some_and(|sender| sender.try_send(job).is_ok());
        if !sent {
            scheduled.remove(&record.content_identity);
        }
        sent
    }
}

impl Drop for RecognitionService {
    fn drop(&mut self) {
        self.sender
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(thread) = self
            .thread
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = thread.join();
        }
    }
}

fn run_worker(
    receiver: mpsc::Receiver<RecognitionJob>,
    persistence: Arc<PersistenceWorker>,
    scheduled: Arc<Mutex<HashSet<ContentIdentity>>>,
    on_saved: Arc<dyn Fn() + Send + Sync>,
) {
    while let Ok(job) = receiver.recv() {
        let decoded = image::load_from_memory(&job.image);
        let result = decoded.map(|image| recognize_image(&image, job.options));
        let (ocr, qr, status) = match result {
            Ok((ocr, qr)) => (ocr, qr, "complete"),
            Err(_) => (None, None, "failed"),
        };
        if persistence
            .save_recognition(job.id, ocr, qr, status)
            .is_ok()
        {
            on_saved();
        }
        lock_unpoisoned(&scheduled).remove(&job.identity);
    }
}

fn recognize_image(
    image: &DynamicImage,
    options: RecognitionOptions,
) -> (Option<String>, Option<String>) {
    let qr = options.qr.then(|| recognize_qr(image)).flatten();
    let ocr = options.ocr.then(|| recognize_ocr(image)).flatten();
    (ocr, qr)
}

fn recognize_qr(image: &DynamicImage) -> Option<String> {
    let mut prepared = rqrr::PreparedImage::prepare(image.to_luma8());
    let values = prepared
        .detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, value)| value))
        .collect::<Vec<_>>();
    normalize_output(&values.join("\n"), MAX_QR_BYTES)
}

fn recognize_ocr(image: &DynamicImage) -> Option<String> {
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, ImageFormat::Png).ok()?;
    crate::platform::windows::ocr::recognize_png(png.get_ref())
        .ok()
        .and_then(|value| normalize_output(&value, MAX_OCR_BYTES))
}

fn image_bytes(record: &ClipboardRecord) -> Option<Vec<u8>> {
    record
        .representations
        .iter()
        .find_map(|representation| match representation {
            ClipboardRepresentation::Png { bytes } => Some(bytes.clone()),
            ClipboardRepresentation::DibV5 { bytes } => dib_to_bmp(bytes),
            _ => None,
        })
}

fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let header_size = usize::try_from(u32::from_le_bytes(dib.get(0..4)?.try_into().ok()?)).ok()?;
    if header_size < 40 || header_size > dib.len() {
        return None;
    }
    let bits = usize::from(u16::from_le_bytes(dib.get(14..16)?.try_into().ok()?));
    let colors = usize::try_from(u32::from_le_bytes(dib.get(32..36)?.try_into().ok()?)).ok()?;
    let palette = if colors > 0 {
        colors
    } else if bits <= 8 {
        1usize.checked_shl(bits as u32)?
    } else {
        0
    };
    let offset = 14usize
        .checked_add(header_size)?
        .checked_add(palette.checked_mul(4)?)?;
    let size = 14usize.checked_add(dib.len())?;
    let mut bmp = Vec::with_capacity(size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&u32::try_from(size).ok()?.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&u32::try_from(offset).ok()?.to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

fn normalize_output(value: &str, max_bytes: usize) -> Option<String> {
    let value = value.replace("\r\n", "\n").replace('\r', "\n");
    let filtered = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut end = trimmed.len().min(max_bytes);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(trimmed[..end].to_owned())
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{MAX_OCR_BYTES, normalize_output};

    #[test]
    fn output_is_normalized_filtered_and_bounded() {
        assert_eq!(
            normalize_output(" a\r\nb\u{0}c ", 32).as_deref(),
            Some("a\nbc")
        );
        let value = "界".repeat(MAX_OCR_BYTES);
        let normalized = normalize_output(&value, MAX_OCR_BYTES).unwrap();
        assert!(normalized.len() <= MAX_OCR_BYTES);
        assert!(normalized.is_char_boundary(normalized.len()));
    }

    #[test]
    fn blank_output_is_discarded() {
        assert_eq!(normalize_output(" \r\n\t ", 100), None);
    }
}
