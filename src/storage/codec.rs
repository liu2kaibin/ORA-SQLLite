use crate::{DbError, ErrorCode, Result};

pub(super) struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub(super) const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(super) fn put_u8(&mut self, value: u8) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_i16(&mut self, value: i16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn put_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(super) struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    pub(super) const fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }

    pub(super) fn get_u8(&mut self) -> Result<u8> {
        Ok(u8::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.get_array()?))
    }

    pub(super) fn get_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(truncated_disk_value)?;
        let bytes = self
            .input
            .get(self.position..end)
            .ok_or_else(truncated_disk_value)?;
        self.position = end;
        Ok(bytes)
    }

    pub(super) const fn position(&self) -> usize {
        self.position
    }

    pub(super) const fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    fn get_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.get_bytes(N)?
            .try_into()
            .map_err(|_| truncated_disk_value())
    }
}

fn truncated_disk_value() -> DbError {
    DbError::new(ErrorCode::Corruption, "truncated disk value")
}

#[test]
fn numeric_round_trip_is_little_endian() {
    let mut encoder = Encoder::new();
    encoder.put_u16(0x1234);
    encoder.put_u64(0x0102_0304_0506_0708);
    let bytes = encoder.into_bytes();
    assert_eq!(&bytes[..2], &[0x34, 0x12]);
    assert_eq!(
        &bytes[2..],
        &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
    );
    let mut decoder = Decoder::new(&bytes);
    assert_eq!(decoder.get_u16().expect("u16"), 0x1234);
    assert_eq!(decoder.get_u64().expect("u64"), 0x0102_0304_0506_0708);
    assert_eq!(decoder.remaining(), 0);
}

#[test]
fn truncated_input_returns_corruption() {
    let mut decoder = Decoder::new(&[1, 2, 3]);
    let error = decoder.get_u64().expect_err("truncated data must fail");
    assert_eq!(error.code(), crate::ErrorCode::Corruption);
    assert_eq!(error.to_string(), "[1700] truncated disk value");
}

#[test]
fn remaining_numeric_widths_and_bytes_round_trip() {
    let mut encoder = Encoder::new();
    encoder.put_u8(0xa5);
    encoder.put_i16(-2);
    encoder.put_u32(0x1234_5678);
    encoder.put_u128(0x0102_0304_0506_0708_1112_1314_1516_1718);
    encoder.put_bytes(&[0xde, 0xad]);

    let bytes = encoder.into_bytes();
    assert_eq!(
        bytes,
        [
            0xa5, 0xfe, 0xff, 0x78, 0x56, 0x34, 0x12, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12,
            0x11, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0xde, 0xad,
        ]
    );
    let mut decoder = Decoder::new(&bytes);
    assert_eq!(decoder.position(), 0);
    assert_eq!(decoder.get_u8().expect("u8"), 0xa5);
    assert_eq!(decoder.get_i16().expect("i16"), -2);
    assert_eq!(decoder.get_u32().expect("u32"), 0x1234_5678);
    assert_eq!(
        decoder.get_u128().expect("u128"),
        0x0102_0304_0506_0708_1112_1314_1516_1718
    );
    assert_eq!(decoder.get_bytes(2).expect("bytes"), &[0xde, 0xad]);
    assert_eq!(decoder.position(), bytes.len());
    assert_eq!(decoder.remaining(), 0);
}

#[test]
fn overflowing_range_returns_corruption_without_advancing() {
    let mut decoder = Decoder::new(&[0xa5]);
    assert_eq!(decoder.get_u8().expect("u8"), 0xa5);

    let error = decoder
        .get_bytes(usize::MAX)
        .expect_err("overflowing range must fail");

    assert_eq!(error.code(), crate::ErrorCode::Corruption);
    assert_eq!(decoder.position(), 1);
}
