mod checksum;
mod codec;

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;

    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0x82f6_3b78;
            } else {
                crc >>= 1;
            }
        }
    }

    !crc
}
