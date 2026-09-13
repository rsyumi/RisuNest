use qrcode::{Color, EcLevel, QrCode};

/// Complete symbol with a four-module quiet zone. Caller sets black on white.
pub fn registration_qr(uri: &str, capable: bool, columns: u16, rows: u16) -> Option<String> {
    if !capable || columns == 0 || rows == 0 {
        return None;
    }
    let qr = QrCode::with_error_correction_level(uri.as_bytes(), EcLevel::M).ok()?;
    let width = qr.width() + 8;
    let height = width.div_ceil(2);
    let link_lines = uri.chars().count().div_ceil(columns as usize);
    if width > columns as usize || height + link_lines + 6 > rows as usize {
        return None;
    }
    let dark = |x: usize, y: usize| {
        x >= 4 && y >= 4 && x < width - 4 && y < width - 4 && qr[(x - 4, y - 4)] == Color::Dark
    };
    let mut result = String::new();
    for y in (0..width).step_by(2) {
        for x in 0..width {
            result.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                _ => ' ',
            });
        }
        result.push('\n');
    }
    Some(result)
}

pub fn safe_text(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qr_never_crops_and_falls_back_without_terminal_support() {
        let uri = "risunestlocal://sync-server/register#synthetic";
        assert!(registration_qr(uri, false, 200, 100).is_none());
        assert!(registration_qr(uri, true, 10, 100).is_none());
        assert!(registration_qr(uri, true, 200, 10).is_none());
        let text = registration_qr(uri, true, 200, 100).unwrap();
        let width = text.lines().next().unwrap().chars().count();
        assert!(text.lines().all(|line| line.chars().count() == width));
        assert!(text.lines().next().unwrap().chars().all(|c| c == ' '));
        assert!(registration_qr(uri, true, (width - 1) as u16, 100).is_none());
    }
    #[test]
    fn terminal_control_sequences_are_not_printed() {
        assert_eq!(safe_text("a\u{1b}\r\nb"), "ab");
    }
}
