use windows::{
    Graphics::Imaging::BitmapDecoder,
    Media::Ocr::OcrEngine,
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize},
};

pub(crate) fn recognize_png(bytes: &[u8]) -> Result<String, String> {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|error| error.to_string())?;
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
    let engine =
        OcrEngine::TryCreateFromUserProfileLanguages().map_err(|error| error.to_string())?;
    engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .and_then(|result| result.Text())
        .map(|text| text.to_string())
        .map_err(|error| error.to_string())
}

pub(crate) fn installed_language_available() -> bool {
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_ok()
        && OcrEngine::TryCreateFromUserProfileLanguages().is_ok()
}
