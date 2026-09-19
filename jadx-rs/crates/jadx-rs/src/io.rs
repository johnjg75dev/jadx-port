//! Positional binary readers.
//!
//! Direct port of jadx's `jadx.plugins.input.java.data.DataReader` (big endian,
//! for `.class` files) plus the little endian + LEB128 accessors jadx needs for
//! DEX (`jadx.plugins.input.dex.internal.DexData`).

use crate::error::{JadxError, Res};

/// Byte order used by [`BinReader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
	Big,
	Little,
}

/// Bounds-checked cursor over a byte slice.
///
/// Every read either advances the cursor or returns
/// `ErrorKind::InputFormat` with the offset at which the data ran out; it never
/// panics, which is what makes it safe to use on untrusted input from a shared
/// library entry point.
#[derive(Debug, Clone)]
pub struct BinReader<'a> {
	data: &'a [u8],
	pos: usize,
	endian: Endian,
}

impl<'a> BinReader<'a> {
	pub fn new(data: &'a [u8]) -> Self {
		BinReader { data, pos: 0, endian: Endian::Big }
	}

	pub fn little(data: &'a [u8]) -> Self {
		BinReader { data, pos: 0, endian: Endian::Little }
	}

	pub fn with_endian(data: &'a [u8], endian: Endian) -> Self {
		BinReader { data, pos: 0, endian }
	}

	pub fn endian(&self) -> Endian {
		self.endian
	}

	pub fn set_endian(&mut self, endian: Endian) {
		self.endian = endian;
	}

	/// Current absolute offset (jadx: `getOffset`).
	pub fn pos(&self) -> usize {
		self.pos
	}

	pub fn set_pos(&mut self, pos: usize) -> Res<()> {
		if pos > self.data.len() {
			return Err(JadxError::format(format!("seek {} past end of data ({})", pos, self.data.len())));
		}
		self.pos = pos;
		Ok(())
	}

	/// jadx: `absPos(int)`.
	pub fn abs_pos(&mut self, pos: usize) -> Res<&mut Self> {
		self.set_pos(pos)?;
		Ok(self)
	}

	pub fn data(&self) -> &'a [u8] {
		self.data
	}

	pub fn len(&self) -> usize {
		self.data.len()
	}

	pub fn is_empty(&self) -> bool {
		self.data.is_empty()
	}

	pub fn remaining(&self) -> usize {
		self.data.len().saturating_sub(self.pos)
	}

	pub fn eof(&self) -> bool {
		self.remaining() == 0
	}

	fn need(&self, n: usize) -> Res<usize> {
		let start = self.pos;
		let end = start.checked_add(n).ok_or_else(|| JadxError::format("read length overflow"))?;
		if end > self.data.len() {
			return Err(JadxError::format(format!(
				"unexpected end of data: want {} byte(s) at offset {}, only {} available",
				n,
				start,
				self.data.len().saturating_sub(start)
			)));
		}
		Ok(start)
	}

	/// jadx: `skip(int)`.
	pub fn skip(&mut self, n: usize) -> Res<()> {
		self.need(n)?;
		self.pos += n;
		Ok(())
	}

	pub fn u8(&mut self) -> Res<u8> {
		let p = self.need(1)?;
		self.pos = p + 1;
		Ok(self.data[p])
	}

	/// Signed byte, jadx: `readS1`.
	pub fn i8(&mut self) -> Res<i8> {
		Ok(self.u8()? as i8)
	}

	pub fn u16(&mut self) -> Res<u16> {
		let p = self.need(2)?;
		self.pos = p + 2;
		let (a, b) = (self.data[p], self.data[p + 1]);
		Ok(match self.endian {
			Endian::Big => ((a as u16) << 8) | b as u16,
			Endian::Little => ((b as u16) << 8) | a as u16,
		})
	}

	/// jadx: `readS2`.
	pub fn i16(&mut self) -> Res<i16> {
		Ok(self.u16()? as i16)
	}

	pub fn u32(&mut self) -> Res<u32> {
		let p = self.need(4)?;
		self.pos = p + 4;
		let d = self.data;
		Ok(match self.endian {
			Endian::Big => u32::from_be_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]),
			Endian::Little => u32::from_le_bytes([d[p], d[p + 1], d[p + 2], d[p + 3]]),
		})
	}

	/// jadx: `readS4`.
	pub fn i32(&mut self) -> Res<i32> {
		Ok(self.u32()? as i32)
	}

	pub fn u64(&mut self) -> Res<u64> {
		let hi = self.u32()? as u64;
		let lo = self.u32()? as u64;
		Ok((hi << 32) | lo)
	}

	pub fn i64(&mut self) -> Res<i64> {
		Ok(self.u64()? as i64)
	}

	pub fn f32(&mut self) -> Res<f32> {
		Ok(f32::from_bits(self.u32()?))
	}

	pub fn f64(&mut self) -> Res<f64> {
		Ok(f64::from_bits(self.u64()?))
	}

	/// Borrowed slice of `n` bytes (jadx: `readBytes`, without the copy).
	pub fn bytes(&mut self, n: usize) -> Res<&'a [u8]> {
		let p = self.need(n)?;
		self.pos = p + n;
		Ok(&self.data[p..p + n])
	}

	/// Owned copy, for callers that outlive the input buffer.
	pub fn bytes_owned(&mut self, n: usize) -> Res<Vec<u8>> {
		Ok(self.bytes(n)?.to_vec())
	}

	/// Read `n` bytes without advancing (constant pool random access).
	pub fn peek_bytes(&self, at: usize, n: usize) -> Res<&'a [u8]> {
		let end = at.checked_add(n).ok_or_else(|| JadxError::format("read length overflow"))?;
		if end > self.data.len() {
			return Err(JadxError::format(format!("unexpected end of data at offset {}", at)));
		}
		Ok(&self.data[at..end])
	}

	pub fn peek_u8(&self, at: usize) -> Res<u8> {
		Ok(self.peek_bytes(at, 1)?[0])
	}

	pub fn peek_u16(&self, at: usize) -> Res<u16> {
		let b = self.peek_bytes(at, 2)?;
		Ok(match self.endian {
			Endian::Big => ((b[0] as u16) << 8) | b[1] as u16,
			Endian::Little => ((b[1] as u16) << 8) | b[0] as u16,
		})
	}

	pub fn peek_u32(&self, at: usize) -> Res<u32> {
		let b = self.peek_bytes(at, 4)?;
		Ok(match self.endian {
			Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
			Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
		})
	}

	/// Unsigned LEB128 (DEX `uleb128`).
	pub fn uleb128(&mut self) -> Res<u32> {
		let mut result: u32 = 0;
		let mut shift = 0u32;
		for i in 0..5u32 {
			let b = self.u8()?;
			result |= ((b & 0x7f) as u32) << shift;
			if b & 0x80 == 0 {
				return Ok(result);
			}
			shift += 7;
			if i == 4 {
				return Err(JadxError::format("malformed uleb128: more than 5 bytes"));
			}
		}
		Err(JadxError::format("malformed uleb128"))
	}

	/// Unsigned LEB128 widened to 64 bit (DEX `uleb128p1` payloads, sizes).
	pub fn uleb128_u64(&mut self) -> Res<u64> {
		let mut result: u64 = 0;
		let mut shift = 0u32;
		loop {
			let b = self.u8()?;
			result |= ((b & 0x7f) as u64) << shift;
			if b & 0x80 == 0 {
				return Ok(result);
			}
			shift += 7;
			if shift > 63 {
				return Err(JadxError::format("malformed uleb128: too long"));
			}
		}
	}

	/// `uleb128p1`: value + 1 encoded.
	pub fn uleb128p1(&mut self) -> Res<u32> {
		Ok(self.uleb128()?.wrapping_sub(1))
	}

	/// Signed LEB128, up to 32 bit (DEX `sleb128`).
	pub fn sleb128(&mut self) -> Res<i32> {
		let mut result: i32 = 0;
		let mut shift = 0u32;
		loop {
			let b = self.u8()?;
			result |= ((b & 0x7f) as i32) << shift;
			shift += 7;
			if b & 0x80 == 0 {
				if shift < 32 && (b & 0x40) != 0 {
					result |= -1i32 << shift;
				}
				return Ok(result);
			}
			if shift >= 35 {
				return Err(JadxError::format("malformed sleb128: too long"));
			}
		}
	}

	/// Signed 32-bit LEB128 used for high registers (`sleb128` of 1 byte in
	/// `const/high16`) -- kept separate to document intent at call sites.
	pub fn sleb128_u32_bits(&mut self) -> Res<i32> {
		self.sleb128()
	}
}

/// Java's `DataInputStream`/`jadx` read `readU2` for a raw u16 index.
pub fn u16_at(data: &[u8], at: usize) -> Res<u16> {
	BinReader::new(data).peek_u16(at)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn big_endian_reads() {
		let d = [0x12u8, 0x34, 0x56, 0x78];
		let mut r = BinReader::new(&d);
		assert_eq!(r.u16().unwrap(), 0x1234);
		assert_eq!(r.u16().unwrap(), 0x5678);
		assert!(r.eof());
		let mut r = BinReader::new(&d);
		assert_eq!(r.u32().unwrap(), 0x1234_5678);
	}

	#[test]
	fn little_endian_reads() {
		let d = [0x78u8, 0x56, 0x34, 0x12];
		let mut r = BinReader::little(&d);
		assert_eq!(r.u32().unwrap(), 0x1234_5678);
	}

	#[test]
	fn truncated_read_is_error() {
		let d = [0x01u8];
		let mut r = BinReader::new(&d);
		assert_eq!(r.u16().unwrap_err().kind, crate::error::ErrorKind::InputFormat);
	}

	#[test]
	fn leb128_longest_value() {
		// 5 bytes: 0x0f << 28
		let d = [0x80u8, 0x80, 0x80, 0x80, 0x0f];
		let mut r = BinReader::new(&d);
		assert_eq!(r.uleb128().unwrap(), 0x0f00_0000u32);
	}

	#[test]
	fn leb128_small_values() {
		let d = [0x00u8, 0x01, 0x7f, 0x80, 0x01];
		let mut r = BinReader::new(&d);
		assert_eq!(r.uleb128().unwrap(), 0);
		assert_eq!(r.uleb128().unwrap(), 1);
		assert_eq!(r.uleb128().unwrap(), 0x7f);
		assert_eq!(r.uleb128().unwrap(), 0x80);
	}

	#[test]
	fn signed_leb() {
		// -1 as a single byte 0x7f
		let d = [0x7fu8];
		let mut r = BinReader::new(&d);
		assert_eq!(r.sleb128().unwrap(), -1);
		// 300 -> 0xac 0x02
		let d = [0xacu8, 0x02];
		let mut r = BinReader::new(&d);
		assert_eq!(r.sleb128().unwrap(), 300);
	}

	#[test]
	fn uleb5_byte_limit() {
		let d = [0x80u8, 0x80, 0x80, 0x80, 0x80];
		let mut r = BinReader::new(&d);
		assert!(r.uleb128().is_err());
	}
}
