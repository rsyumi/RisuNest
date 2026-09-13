mod placement;

use serde::{Deserialize, Serialize};
use std::{io::Write, mem::size_of, path::PathBuf, sync::Mutex};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime, Theme, WebviewWindow, Window,
};
use windows::Win32::{
    Foundation::COLORREF,
    Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR},
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        },
    },
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Appearance {
    background: String,
    caption: String,
    text: String,
    dark: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            background: "#282a36".into(),
            caption: "#21222c".into(),
            text: "#f8f8f2".into(),
            dark: true,
        }
    }
}

fn rgb(value: &str) -> Result<[u8; 3], String> {
    if value.len() != 7
        || !value.starts_with('#')
        || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err("Invalid Windows appearance color".into());
    }
    let value =
        u32::from_str_radix(&value[1..], 16).map_err(|_| "Invalid Windows appearance color")?;
    Ok([(value >> 16) as u8, (value >> 8) as u8, value as u8])
}

impl Appearance {
    fn validate(&self) -> Result<(), String> {
        rgb(&self.background)?;
        rgb(&self.caption)?;
        rgb(&self.text)?;
        Ok(())
    }

    fn color(&self, startup: bool) -> tauri::window::Color {
        let [r, g, b] = rgb(if startup {
            &self.caption
        } else {
            &self.background
        })
        .expect("validated appearance");
        tauri::window::Color(r, g, b, 255)
    }
}

struct AppearanceState {
    current: Mutex<Appearance>,
    cache: PathBuf,
}

impl AppearanceState {
    fn snapshot(&self) -> Option<Appearance> {
        self.current.lock().ok().map(|value| value.clone())
    }
}

fn contrast_theme_enabled() -> bool {
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some(&mut contrast as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
            && contrast.dwFlags.0 & HCF_HIGHCONTRASTON.0 != 0
    }
}

fn apply_caption<R: Runtime>(window: &Window<R>, appearance: &Appearance) -> tauri::Result<()> {
    let hwnd = window.hwnd()?;
    let contrast = contrast_theme_enabled();
    for (attribute, color) in [
        (DWMWA_CAPTION_COLOR, &appearance.caption),
        (DWMWA_TEXT_COLOR, &appearance.text),
    ] {
        let [r, g, b] = rgb(color).expect("validated appearance");
        let color = COLORREF(if contrast {
            0xffff_ffff
        } else {
            r as u32 | (g as u32) << 8 | (b as u32) << 16
        });
        // Windows versions without these attributes keep their standard caption.
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                attribute,
                &color as *const _ as *const _,
                size_of::<COLORREF>() as u32,
            );
        }
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn windows_set_appearance(
    window: WebviewWindow,
    appearance: Appearance,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Windows appearance is limited to the main window".into());
    }
    appearance.validate()?;
    window
        .set_theme(Some(if appearance.dark {
            Theme::Dark
        } else {
            Theme::Light
        }))
        .map_err(|e| e.to_string())?;
    window
        .set_background_color(Some(appearance.color(false)))
        .map_err(|e| e.to_string())?;
    apply_caption(&window.as_ref().window(), &appearance).map_err(|e| e.to_string())?;
    let state = window.state::<AppearanceState>();
    let mut current = state
        .current
        .lock()
        .map_err(|_| "Windows appearance state unavailable")?;
    if *current != appearance || !state.cache.exists() {
        // The cache is disposable. Failure to cache must not reject a visible theme update.
        let save = || -> std::io::Result<()> {
            let parent = state.cache.parent().expect("cache parent");
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(&serde_json::to_vec(&appearance)?)?;
            file.persist(&state.cache).map_err(|e| e.error)?;
            Ok(())
        };
        if save().is_err() {
            crate::nlog!("warn", "Could not cache Windows startup colors");
        }
        *current = appearance;
    }
    Ok(())
}

pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("windows-appearance")
        .js_init_script(include_str!("windows_appearance/startup.js").to_owned())
        .setup(|app, _| {
            let cache = app.path().app_config_dir()?.join("windows-appearance.json");
            let mut appearance = Appearance::default();
            match std::fs::read(&cache) {
                Ok(bytes) => match serde_json::from_slice::<Appearance>(&bytes) {
                    Ok(value) if value.validate().is_ok() => appearance = value,
                    _ => crate::nlog!("warn", "Invalid Windows startup color cache"),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => crate::nlog!("warn", "Could not read Windows startup colors"),
            }
            app.manage(AppearanceState {
                current: Mutex::new(appearance),
                cache,
            });
            Ok(())
        })
        .on_window_ready(|window| {
            if window.label() != "main" {
                return;
            }
            let state = window.state::<AppearanceState>();
            if let Some(appearance) = state.snapshot() {
                let _ = window.set_theme(Some(if appearance.dark {
                    Theme::Dark
                } else {
                    Theme::Light
                }));
                let _ = window.set_background_color(Some(appearance.color(true)));
                let _ = apply_caption(&window, &appearance);
            }
        })
        .on_webview_ready(|webview| {
            if webview.label() != "main" {
                return;
            }
            let state = webview.state::<AppearanceState>();
            if let Some(appearance) = state.snapshot() {
                let _ = webview.set_background_color(Some(appearance.color(true)));
            }
            // Window-state has restored normal/maximized geometry before this hook.
            let window = webview.window();
            if placement::keep_visible(&window).is_err() {
                crate::nlog!("warn", "Could not restore Windows work-area bounds");
            }
            let _ = window.show();
        })
        .on_event(|app, event| {
            if let tauri::RunEvent::WindowEvent {
                label,
                event: tauri::WindowEvent::Focused(_) | tauri::WindowEvent::ThemeChanged(_),
                ..
            } = event
            {
                if label != "main" {
                    return;
                }
                if let Some(window) = app.get_webview_window(label) {
                    let state = app.state::<AppearanceState>();
                    if let Some(appearance) = state.snapshot() {
                        let _ = apply_caption(&window.as_ref().window(), &appearance);
                    }
                }
            }
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_opaque_rgb_reaches_dwm() {
        assert_eq!(rgb("#Ab12ef").unwrap(), [171, 18, 239]);
        for invalid in ["red", "#123", "#12345678", "#gg1234", "#あいう", "url(x)"] {
            assert!(rgb(invalid).is_err());
        }
    }

    #[test]
    fn cache_requires_current_shape_and_valid_colors() {
        let value = Appearance::default();
        assert_eq!(
            serde_json::from_slice::<Appearance>(&serde_json::to_vec(&value).unwrap()).unwrap(),
            value
        );
        assert!(serde_json::from_str::<Appearance>(
            r##"{"background":"#000000","caption":"#000000","text":"#ffffff"}"##
        )
        .is_err());
        let mut invalid = value;
        invalid.caption = "transparent".into();
        assert!(invalid.validate().is_err());
    }
}
