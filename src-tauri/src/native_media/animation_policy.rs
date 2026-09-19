use super::{InlayAnimation, MAX_ANIMATION_FRAMES, MAX_ANIMATION_RGBA_BYTES};

fn take<'a>(data: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let value = data.get(..len)?;
    *data = &data[len..];
    Some(value)
}

fn le16(data: &[u8]) -> u64 { u16::from_le_bytes(data[..2].try_into().unwrap()) as u64 }
fn le24(data: &[u8]) -> u64 { u32::from_le_bytes([data[0], data[1], data[2], 0]) as u64 }
fn be32(data: &[u8]) -> u64 { u32::from_be_bytes(data[..4].try_into().unwrap()) as u64 }

fn sub_blocks(data: &mut &[u8]) -> Option<()> {
    loop {
        let len = take(data, 1)?[0] as usize;
        if len == 0 { return Some(()); }
        take(data, len)?;
    }
}

// Count compressed frame records without allocating pixel buffers.
fn dimensions(data: &[u8], source: InlayAnimation) -> Option<(u64, u64, u64)> {
    match source {
        InlayAnimation::Gif => {
            let mut data = data;
            let header = take(&mut data, 13)?;
            let mut width = le16(&header[6..]);
            let mut height = le16(&header[8..]);
            if header[10] & 0x80 != 0 { take(&mut data, 3 << ((header[10] & 7) + 1))?; }
            let mut frames = 0;
            loop {
                match take(&mut data, 1)?[0] {
                    0x3b => return Some((width, height, frames)),
                    0x21 => { take(&mut data, 1)?; sub_blocks(&mut data)?; }
                    0x2c => {
                        let frame = take(&mut data, 9)?;
                        width = width.max(le16(frame) + le16(&frame[4..]));
                        height = height.max(le16(&frame[2..]) + le16(&frame[6..]));
                        if frame[8] & 0x80 != 0 { take(&mut data, 3 << ((frame[8] & 7) + 1))?; }
                        take(&mut data, 1)?;
                        sub_blocks(&mut data)?;
                        frames += 1;
                    }
                    _ => return None,
                }
            }
        }
        InlayAnimation::Apng => {
            let mut data = data.get(8..)?;
            let (mut width, mut height, mut declared, mut frames) = (0, 0, 0, 0);
            loop {
                let header = take(&mut data, 8)?;
                let body = take(&mut data, be32(header) as usize)?;
                take(&mut data, 4)?;
                match &header[4..] {
                    b"IHDR" if body.len() == 13 => { width = be32(body); height = be32(&body[4..]); }
                    b"acTL" if body.len() == 8 => declared = be32(body),
                    b"fcTL" if body.len() == 26 => {
                        width = width.max(be32(&body[4..]) + be32(&body[12..]));
                        height = height.max(be32(&body[8..]) + be32(&body[16..]));
                        frames += 1;
                    }
                    b"IEND" => return Some((width, height, frames.max(declared))),
                    _ => {}
                }
            }
        }
        InlayAnimation::WebP => {
            let length = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
            if length.checked_add(8)? != data.len() { return None; }
            let mut data = data.get(12..)?;
            let (mut width, mut height, mut frames) = (0, 0, 0);
            while !data.is_empty() {
                let header = take(&mut data, 8)?;
                let len = u32::from_le_bytes(header[4..].try_into().ok()?) as usize;
                let body = take(&mut data, len)?;
                if len & 1 != 0 { take(&mut data, 1)?; }
                match &header[..4] {
                    b"VP8X" if body.len() == 10 => { width = le24(&body[4..]) + 1; height = le24(&body[7..]) + 1; }
                    b"ANMF" if body.len() >= 16 => {
                        width = width.max(2 * le24(body) + le24(&body[6..]) + 1);
                        height = height.max(2 * le24(&body[3..]) + le24(&body[9..]) + 1);
                        frames += 1;
                    }
                    _ => {}
                }
            }
            Some((width, height, frames))
        }
    }
}

pub(super) fn permits_decode(data: &[u8], source: InlayAnimation, budget: u64) -> bool {
    let Some((width, height, frames)) = dimensions(data, source) else { return false; };
    if width == 0 || height == 0 || frames == 0 || frames > MAX_ANIMATION_FRAMES as u64 { return false; }
    // Reserve two additional canvases for composition. This is a decode estimate, not an RSS cap.
    width.checked_mul(height).and_then(|v| v.checked_mul(4))
        .and_then(|v| v.checked_mul(frames + 2))
        .is_some_and(|cost| cost <= budget.min(MAX_ANIMATION_RGBA_BYTES))
}
