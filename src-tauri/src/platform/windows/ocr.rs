use std::sync::OnceLock;

use windows::{
    Graphics::Imaging::BitmapDecoder,
    Media::Ocr::OcrEngine,
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
    Win32::Foundation::RPC_E_CHANGED_MODE,
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
};

fn initialize_winrt() -> Result<(), String> {
    match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        Ok(()) => Ok(()),
        Err(error) if error.code() == RPC_E_CHANGED_MODE => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn create_engine() -> Result<OcrEngine, String> {
    if let Ok(engine) = OcrEngine::TryCreateFromUserProfileLanguages() {
        return Ok(engine);
    }
    let languages = OcrEngine::AvailableRecognizerLanguages().map_err(|error| error.to_string())?;
    let count = languages.Size().map_err(|error| error.to_string())?;
    for index in 0..count {
        let language = languages.GetAt(index).map_err(|error| error.to_string())?;
        if let Ok(engine) = OcrEngine::TryCreateFromLanguage(&language) {
            return Ok(engine);
        }
    }
    Err("no Windows OCR language is available".to_owned())
}

pub(crate) fn recognize_png(bytes: &[u8]) -> Result<String, String> {
    initialize_winrt()?;
    let stream = InMemoryRandomAccessStream::new().map_err(|error| error.to_string())?;
    let writer = DataWriter::CreateDataWriter(&stream).map_err(|error| error.to_string())?;
    writer
        .WriteBytes(bytes)
        .map_err(|error| error.to_string())?;
    writer
        .StoreAsync()
        .and_then(|operation| operation.get())
        .map_err(|error| error.to_string())?;
    writer.DetachStream().map_err(|error| error.to_string())?;
    stream.Seek(0).map_err(|error| error.to_string())?;
    let decoder = BitmapDecoder::CreateAsync(&stream)
        .and_then(|operation| operation.get())
        .map_err(|error| error.to_string())?;
    let bitmap = decoder
        .GetSoftwareBitmapAsync()
        .and_then(|operation| operation.get())
        .map_err(|error| error.to_string())?;
    let engine = create_engine()?;
    engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .and_then(|result| result.Text())
        .map(|text| text.to_string())
        .map_err(|error| error.to_string())
}

pub(crate) fn installed_language_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::thread::Builder::new()
            .name("ocr-capability-check".to_owned())
            .spawn(|| initialize_winrt().is_ok() && create_engine().is_ok())
            .ok()
            .and_then(|thread| thread.join().ok())
            .unwrap_or(false)
    })
}
