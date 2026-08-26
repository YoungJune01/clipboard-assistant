use std::path::Path;

use windows::{
    Win32::{
        Media::Audio::*, System::Diagnostics::Debug::MessageBeep, UI::WindowsAndMessaging::MB_OK,
    },
    core::{HSTRING, PCWSTR},
};

pub fn play_default() -> Result<(), String> {
    unsafe { MessageBeep(MB_OK) }.map_err(|error| error.to_string())
}

pub fn play_file(path: &Path) -> Result<(), String> {
    let path = HSTRING::from(path.as_os_str());
    let played = unsafe {
        PlaySoundW(
            PCWSTR(path.as_ptr()),
            None,
            SND_ASYNC | SND_FILENAME | SND_NODEFAULT,
        )
    };
    played.ok().map_err(|error| error.to_string())
}
