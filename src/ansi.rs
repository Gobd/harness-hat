pub(crate) fn ansi_16_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (0x00, 0x00, 0x00),
        1 => (0xcd, 0x00, 0x00),
        2 => (0x00, 0xcd, 0x00),
        3 => (0xcd, 0xcd, 0x00),
        4 => (0x00, 0x00, 0xee),
        5 => (0xcd, 0x00, 0xcd),
        6 => (0x00, 0xcd, 0xcd),
        7 => (0xe5, 0xe5, 0xe5),
        8 => (0x7f, 0x7f, 0x7f),
        9 => (0xff, 0x00, 0x00),
        10 => (0x00, 0xff, 0x00),
        11 => (0xff, 0xff, 0x00),
        12 => (0x5c, 0x5c, 0xff),
        13 => (0xff, 0x00, 0xff),
        14 => (0x00, 0xff, 0xff),
        _ => (0xff, 0xff, 0xff),
    }
}

pub(crate) fn xterm_256_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => ansi_16_to_rgb(idx),
        16..=231 => {
            let i = idx - 16;
            let r = i / 36;
            let g = (i / 6) % 6;
            let b = i % 6;
            (xterm_cube(r), xterm_cube(g), xterm_cube(b))
        }
        232..=255 => {
            let shade = 8 + (idx - 232) * 10;
            (shade, shade, shade)
        }
    }
}

fn xterm_cube(v: u8) -> u8 {
    match v {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}
