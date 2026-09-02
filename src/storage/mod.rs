mod checksum;
mod codec;
mod header;
mod page;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PageId(pub(super) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Lsn(pub(super) u64);

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
