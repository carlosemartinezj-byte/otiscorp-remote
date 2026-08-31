//! Inyeccion de entrada remota (control de raton y teclado), multiplataforma.
//!
//! El equipo *controlado* recibe eventos del visor y los inyecta con la API
//! nativa: `SendInput` en Windows, `CGEvent` (Quartz) en macOS. Las coordenadas
//! del raton llegan **normalizadas** (0.0..1.0) respecto a la pantalla, porque
//! visor y controlado suelen tener resoluciones distintas.
//!
//! Teclado: el visor envia el codigo de tecla virtual de Windows (`vk`) y ademas
//! el `code` fisico del navegador (p.ej. "KeyA", "Enter", "ArrowLeft"), que es
//! independiente del sistema. Windows usa `vk`; macOS traduce `code` a su
//! `CGKeyCode`.

// ===========================================================================
// Windows: SendInput
// ===========================================================================
#[cfg(windows)]
mod imp {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEINPUT, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
        MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
        MOUSEEVENTF_WHEEL, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
    };

    const ABS_MAX: f64 = 65535.0;

    fn send(input: INPUT) {
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn move_mouse(x_norm: f64, y_norm: f64) {
        let x = (x_norm.clamp(0.0, 1.0) * ABS_MAX).round() as i32;
        let y = (y_norm.clamp(0.0, 1.0) * ABS_MAX).round() as i32;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x,
                    dy: y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn mouse_button(button: &str, down: bool) -> Result<(), String> {
        let flag: MOUSE_EVENT_FLAGS = match (button, down) {
            ("left", true) => MOUSEEVENTF_LEFTDOWN,
            ("left", false) => MOUSEEVENTF_LEFTUP,
            ("right", true) => MOUSEEVENTF_RIGHTDOWN,
            ("right", false) => MOUSEEVENTF_RIGHTUP,
            ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
            ("middle", false) => MOUSEEVENTF_MIDDLEUP,
            _ => return Err(format!("boton no soportado: {button}")),
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
        Ok(())
    }

    pub fn scroll(delta: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    fn is_extended(vk: u16) -> bool {
        matches!(
            vk,
            0x21 | 0x22 | 0x23 | 0x24 | 0x25 | 0x26 | 0x27 | 0x28
                | 0x2D | 0x2E
                | 0xA3 | 0xA5
                | 0x5B | 0x5C
                | 0x90
        )
    }

    pub fn key(vk: u16, _code: &str, down: bool) {
        if vk == 0 {
            return;
        }
        let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
        if is_extended(vk) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn key_unicode(ch: u16, down: bool) {
        let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_UNICODE;
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: ch,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send(input);
    }

    pub fn type_text(text: &str) {
        for unit in text.encode_utf16() {
            key_unicode(unit, true);
            key_unicode(unit, false);
        }
    }
}

// ===========================================================================
// macOS: CGEvent (Quartz). Requiere permiso de Accesibilidad para inyectar.
// ===========================================================================
#[cfg(target_os = "macos")]
mod imp {
    use core_graphics::display::CGDisplay;
    use core_graphics::event::{
        CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use std::sync::Mutex;

    // Ultima posicion del cursor (los eventos de boton necesitan un punto).
    static LAST_POS: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));

    fn source() -> Option<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()
    }

    fn screen_size() -> (f64, f64) {
        let b = CGDisplay::main().bounds();
        (b.size.width, b.size.height)
    }

    pub fn move_mouse(x_norm: f64, y_norm: f64) {
        let (w, h) = screen_size();
        let x = x_norm.clamp(0.0, 1.0) * w;
        let y = y_norm.clamp(0.0, 1.0) * h;
        *LAST_POS.lock().unwrap() = (x, y);
        if let Some(src) = source() {
            if let Ok(ev) = CGEvent::new_mouse_event(
                src,
                CGEventType::MouseMoved,
                CGPoint::new(x, y),
                CGMouseButton::Left,
            ) {
                ev.post(CGEventTapLocation::HID);
            }
        }
    }

    pub fn mouse_button(button: &str, down: bool) -> Result<(), String> {
        let (etype, cgbtn) = match (button, down) {
            ("left", true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            ("left", false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            ("right", true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            ("right", false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            ("middle", true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            ("middle", false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            _ => return Err(format!("boton no soportado: {button}")),
        };
        let (x, y) = *LAST_POS.lock().unwrap();
        if let Some(src) = source() {
            if let Ok(ev) =
                CGEvent::new_mouse_event(src, etype, CGPoint::new(x, y), cgbtn)
            {
                ev.post(CGEventTapLocation::HID);
            }
        }
        Ok(())
    }

    pub fn scroll(delta: i32) {
        // En Windows un "clic" de rueda = 120; en mac usamos lineas (~1 por clic).
        let lines = (delta / 120).clamp(-10, 10);
        let lines = if lines == 0 { delta.signum() } else { lines };
        if let Some(src) = source() {
            if let Ok(ev) =
                CGEvent::new_scroll_event(src, ScrollEventUnit::LINE, 1, lines, 0, 0)
            {
                ev.post(CGEventTapLocation::HID);
            }
        }
    }

    pub fn key(_vk: u16, code: &str, down: bool) {
        if let Some(keycode) = code_to_cgkeycode(code) {
            if let Some(src) = source() {
                if let Ok(ev) = CGEvent::new_keyboard_event(src, keycode, down) {
                    ev.post(CGEventTapLocation::HID);
                }
            }
        }
    }

    pub fn type_text(text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(src) = source() {
            if let Ok(ev) = CGEvent::new_keyboard_event(src, 0, true) {
                ev.set_string(text);
                ev.post(CGEventTapLocation::HID);
            }
        }
    }

    /// Traduce el `code` fisico del navegador al CGKeyCode de macOS (layout ANSI US).
    fn code_to_cgkeycode(code: &str) -> Option<u16> {
        // Letras
        if let Some(c) = code.strip_prefix("Key") {
            let m = match c {
                "A" => 0, "B" => 11, "C" => 8, "D" => 2, "E" => 14, "F" => 3, "G" => 5,
                "H" => 4, "I" => 34, "J" => 38, "K" => 40, "L" => 37, "M" => 46, "N" => 45,
                "O" => 31, "P" => 35, "Q" => 12, "R" => 15, "S" => 1, "T" => 17, "U" => 32,
                "V" => 9, "W" => 13, "X" => 7, "Y" => 16, "Z" => 6,
                _ => return None,
            };
            return Some(m);
        }
        // Digitos fila superior
        if let Some(d) = code.strip_prefix("Digit") {
            let m = match d {
                "1" => 18, "2" => 19, "3" => 20, "4" => 21, "5" => 23, "6" => 22,
                "7" => 26, "8" => 28, "9" => 25, "0" => 29,
                _ => return None,
            };
            return Some(m);
        }
        // Funcion F1..F12
        if let Some(f) = code.strip_prefix('F') {
            if let Ok(n) = f.parse::<u8>() {
                let m = match n {
                    1 => 122, 2 => 120, 3 => 99, 4 => 118, 5 => 96, 6 => 97, 7 => 98,
                    8 => 100, 9 => 101, 10 => 109, 11 => 103, 12 => 111,
                    _ => return None,
                };
                return Some(m);
            }
        }
        let m = match code {
            "Enter" | "NumpadEnter" => 36,
            "Escape" => 53,
            "Backspace" => 51,
            "Tab" => 48,
            "Space" => 49,
            "Minus" => 27,
            "Equal" => 24,
            "BracketLeft" => 33,
            "BracketRight" => 30,
            "Backslash" => 42,
            "Semicolon" => 41,
            "Quote" => 39,
            "Backquote" => 50,
            "Comma" => 43,
            "Period" => 47,
            "Slash" => 44,
            "ArrowLeft" => 123,
            "ArrowRight" => 124,
            "ArrowDown" => 125,
            "ArrowUp" => 126,
            "Delete" => 117,
            "Home" => 115,
            "End" => 119,
            "PageUp" => 116,
            "PageDown" => 121,
            "ShiftLeft" => 56,
            "ShiftRight" => 60,
            "ControlLeft" => 59,
            "ControlRight" => 62,
            "AltLeft" => 58,
            "AltRight" => 61,
            "MetaLeft" => 55,
            "MetaRight" => 54,
            "CapsLock" => 57,
            _ => return None,
        };
        Some(m)
    }
}

// ===========================================================================
// Otras plataformas: no-op
// ===========================================================================
#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    pub fn move_mouse(_x: f64, _y: f64) {}
    pub fn mouse_button(_button: &str, _down: bool) -> Result<(), String> {
        Ok(())
    }
    pub fn scroll(_delta: i32) {}
    pub fn key(_vk: u16, _code: &str, _down: bool) {}
    pub fn type_text(_text: &str) {}
}

// ---- API publica -----------------------------------------------------------
pub fn move_mouse(x_norm: f64, y_norm: f64) {
    imp::move_mouse(x_norm, y_norm);
}
pub fn mouse_button(button: &str, down: bool) -> Result<(), String> {
    imp::mouse_button(button, down)
}
pub fn scroll(delta: i32) {
    imp::scroll(delta);
}
pub fn key(vk: u16, code: &str, down: bool) {
    imp::key(vk, code, down);
}
pub fn type_text(text: &str) {
    imp::type_text(text);
}
