//! Dependency-free zip reader with the DEFLATE decoder of RFC 1951/1952.
//!
//! jadx reads `.jar`/`.aar`/`.apk`/`.zip` through `java.util.zip.ZipFile`
//! (`jadx.core.utils.files.ZipDictionary`, `JadxDbgFilesCollector`...), which of
//! course is only available on a JVM. This port is `std`-only, so the container
//! format is read here: End Of Central Directory -> central directory -> local
//! headers -> `stored` or `deflate` payload.
//!
//! Only what a *reader* needs is implemented:
//!
//! * methods 0 (`stored`) and 8 (`deflate`, raw, no zlib header -- the zip
//!   convention), which cover essentially every jar produced by javac/gradle;
//! * data descriptors (bit 3 of the general flags) are honoured by taking the
//!   sizes from the central directory;
//! * entries are returned in central-directory order;
//! * zip64 EOCD records are detected and reported as `Unsupported` rather than
//!   silently truncated (jadx inherits zip64 support from the JDK).
//!
//! The decompressor writes into a growable buffer with an explicit output limit
//! ([`ZipOptions::max_entry_bytes`]) so a zip bomb cannot exhaust memory.

use crate::error::{ErrorKind, JadxError, Res};

pub const EOCD_SIG: u32 = 0x0605_4b50;
pub const EOCD64_SIG: u32 = 0x0607_4b50;
pub const EOCD64_LOC_SIG: u32 = 0x0706_4b50;
pub const CEN_SIG: u32 = 0x0201_4b50;
pub const LOC_SIG: u32 = 0x0403_4b50;

#[derive(Debug, Clone)]
pub struct ZipEntry {
	pub name: String,
	/// 0 = stored, 8 = deflate
	pub method: u16,
	pub flags: u16,
	pub crc32: u32,
	pub comp_size: u64,
	pub uncomp_size: u64,
	/// offset of the local file header inside the archive buffer
	pub local_offset: u64,
	pub comment: String,
	pub is_dir: bool,
}

impl ZipEntry {
	pub fn is_deflated(&self) -> bool {
		self.method == 8
	}

	/// jadx uses the extension to decide whether a resource is a class file.
	pub fn extension(&self) -> &str {
		match self.name.rfind('.') {
			Some(p) => &self.name[p + 1..],
			None => "",
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub struct ZipOptions {
	/// refuse to inflate more than this many bytes per entry
	pub max_entry_bytes: u64,
	/// verify CRC-32 of every inflated entry
	pub verify_crc: bool,
}

impl Default for ZipOptions {
	fn default() -> Self {
		ZipOptions { max_entry_bytes: 64 * 1024 * 1024, verify_crc: false }
	}
}

#[derive(Debug, Clone)]
pub struct ZipArchive<'a> {
	src: &'a [u8],
	entries: Vec<ZipEntry>,
}

impl<'a> ZipArchive<'a> {
	pub fn entries(&self) -> &[ZipEntry] {
		&self.entries
	}

	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}

	pub fn find(&self, name: &str) -> Option<usize> {
		self.entries.iter().position(|e| e.name == name)
	}

	/// Parse the central directory.
	pub fn open(src: &'a [u8]) -> Res<ZipArchive<'a>> {
		let eocd = find_eocd(src)?;
		let total = read_u16(src, eocd + 10)? as usize;
		let cen_off = read_u32(src, eocd + 16)? as u64;
		let comment_len = read_u16(src, eocd + 20)? as usize;
		let archive_comment = utf8_lossy(&src[eocd + 22..eocd + 22 + comment_len.min(src.len().saturating_sub(eocd + 22))]);
		let mut pos = cen_off as usize;
		let mut entries: Vec<ZipEntry> = Vec::with_capacity(total);
		for n in 0..total {
			if read_u32(src, pos)? != CEN_SIG {
				return Err(err(format!(
					"central directory entry {} of {} is not a valid header (offset {})",
					n + 1,
					total,
					pos
				)));
			}
			let flags = read_u16(src, pos + 8)?;
			let method = read_u16(src, pos + 10)?;
			let crc = read_u32(src, pos + 16)?;
			let mut comp = read_u32(src, pos + 20)? as u64;
			let mut uncomp = read_u32(src, pos + 24)? as u64;
			let name_len = read_u16(src, pos + 28)? as usize;
			let extra_len = read_u16(src, pos + 30)? as usize;
			let cen_comment_len = read_u16(src, pos + 32)? as usize;
			let mut local = read_u32(src, pos + 42)? as u64;
			let name = utf8_lossy(&slice(src, pos + 46, name_len)?);
			let extra = slice(src, pos + 46 + name_len, extra_len)?;
			let comment = utf8_lossy(&slice(src, pos + 46 + name_len + extra_len, cen_comment_len)?);
			// zip64 extended information extra field (tag 0x0001)
			if comp == u32::MAX as u64 || uncomp == u32::MAX as u64 || local == u32::MAX as u64 {
				let (c, u, l) = parse_zip64_extra(extra)?;
				if uncomp == u32::MAX as u64 {
					uncomp = u;
				}
				if comp == u32::MAX as u64 {
					comp = c;
				}
				if local == u32::MAX as u64 {
					local = l;
				}
			}
			entries.push(ZipEntry {
				name,
				method,
				flags,
				crc32: crc,
				comp_size: comp,
				uncomp_size: uncomp,
				local_offset: local,
				comment,
				is_dir: false,
			});
			pos += 46 + name_len + extra_len + cen_comment_len;
		}
		let _ = archive_comment;
		for e in entries.iter_mut() {
			e.is_dir = e.name.ends_with('/');
		}
		Ok(ZipArchive { src, entries })
	}

	/// The compressed bytes of an entry, i.e. after the local header.
	pub fn raw_bytes(&self, index: usize) -> Res<&'a [u8]> {
		let e = self.entry(index)?;
		let p = e.local_offset as usize;
		if read_u32(self.src, p)? != LOC_SIG {
			return Err(err(format!("bad local header for {}", e.name)));
		}
		let name_len = read_u16(self.src, p + 26)? as usize;
		let extra_len = read_u16(self.src, p + 28)? as usize;
		let start = p + 30 + name_len + extra_len;
		// a data descriptor makes the sizes in the local header zero, so trust the
		// central directory (that is why the offset arithmetic above is needed)
		let len = e.comp_size as usize;
		Ok(slice(self.src, start, len)?)
	}

	pub fn entry(&self, index: usize) -> Res<&'a ZipEntry> {
		self.entries.get(index).ok_or_else(|| err(format!("no zip entry #{}", index)))
	}

	/// Decompressed content of an entry.
	pub fn read(&self, index: usize, opts: &ZipOptions) -> Res<Vec<u8>> {
		let e = self.entry(index)?.clone();
		let data = self.raw_bytes(index)?;
		let out = match e.method {
			0 => data.to_vec(),
			8 => inflate_raw(data, e.uncomp_size, opts)?,
			other => {
				return Err(JadxError::new(
					ErrorKind::Unsupported,
					format!("zip entry {} uses unsupported compression method {}", e.name, other),
				))
			}
		};
		if opts.verify_crc && e.crc32 != 0 && crc32(&out) != e.crc32 {
			return Err(err(format!("crc mismatch for zip entry {}", e.name)));
		}
		Ok(out)
	}

	/// Every entry whose name ends with `suffix` (jadx collects `*.class` this
	/// way in `InputFile`).
	pub fn read_all_with_suffix(&self, suffix: &str, opts: &ZipOptions) -> Res<Vec<(String, Vec<u8>)>> {
		let mut out = Vec::new();
		for (i, e) in self.entries.iter().enumerate() {
			if e.is_dir || !e.name.ends_with(suffix) {
				continue;
			}
			out.push((e.name.clone(), self.read(i, opts)?));
		}
		Ok(out)
	}
}

fn err(msg: String) -> JadxError {
	JadxError::format(msg)
}

fn read_u16(src: &[u8], at: usize) -> Res<u16> {
	if at + 2 > src.len() {
		return Err(err(format!("zip file truncated at {}", at)));
	}
	Ok(u16::from_le_bytes([src[at], src[at + 1]]))
}

fn read_u32(src: &[u8], at: usize) -> Res<u32> {
	if at + 4 > src.len() {
		return Err(err(format!("zip file truncated at {}", at)));
	}
	Ok(u32::from_le_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]]))
}

fn read_u64(src: &[u8], at: usize) -> Res<u64> {
	if at + 8 > src.len() {
		return Err(err(format!("zip file truncated at {}", at)));
	}
	let mut b = [0u8; 8];
	b.copy_from_slice(&src[at..at + 8]);
	Ok(u64::from_le_bytes(b))
}

fn slice(src: &[u8], at: usize, len: usize) -> Res<&[u8]> {
	if at.checked_add(len).map(|e| e > src.len()).unwrap_or(true) || at > src.len() {
		return Err(err(format!("zip file truncated: {} + {} > {}", at, len, src.len())));
	}
	Ok(&src[at..at + len])
}

fn utf8_lossy(bytes: &[u8]) -> String {
	String::from_utf8_lossy(bytes).into_owned()
}

/// Scan backwards for `EOCD`; the comment may be up to 64 KiB long, which is the
/// window java.util.zip uses too.
fn find_eocd(src: &[u8]) -> Res<usize> {
	if src.len() < 22 {
		return Err(err("not a zip file: shorter than the end of central directory record".to_string()));
	}
	let max_back = (src.len() - 4).min(22 + u16::MAX as usize);
	let start = src.len() - max_back;
	let mut i = src.len() - 4;
	loop {
		if u32::from_le_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]) == EOCD_SIG {
			// validate the recorded comment length so a byte pattern inside a
			// stored entry is not mistaken for the record
			let clen = u16::from_le_bytes([src[i + 20], src[i + 21]]) as usize;
			if i + 22 + clen <= src.len() {
				return Ok(i);
			}
		}
		if i == start {
			break;
		}
		i -= 1;
	}
	Err(err("not a zip file: end of central directory record not found".to_string()))
}

/// The zip64 "extra field" as `Extra field tag = 0x0001`; fields appear only
/// when the corresponding directory value was 0xFFFFFFFF.
fn parse_zip64_extra(extra: &[u8]) -> Res<(u64, u64, u64)> {
	let mut pos = 0usize;
	while pos + 4 <= extra.len() {
		let tag = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
		let len = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
		let body = pos + 4;
		if body + len > extra.len() {
			break;
		}
		if tag == 0x0001 {
			let mut v = [0u64; 3];
			let mut p = body;
			for slot in v.iter_mut() {
				if p + 8 <= body + len {
					*slot = read_u64(extra, p)?;
					p += 8;
				} else {
					break;
				}
			}
			// order is uncompressed size, compressed size, local header offset
			return Ok((v[1], v[0], v[2]));
		}
		pos = body + len;
	}
	Err(JadxError::new(
		ErrorKind::Unsupported,
		"zip64 entry without a zip64 extended information extra field".to_string(),
	))
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3 polynomial, as used by zip and by java.util.zip.CRC32)
// ---------------------------------------------------------------------------

/// Table-free implementation: 8 shifts per byte is fast enough for class files
/// and avoids the 1 KiB of `static` data a table would need in a `.so`.
pub fn crc32(data: &[u8]) -> u32 {
	let mut crc = !0u32;
	for &b in data {
		crc ^= b as u32;
		for _ in 0..8 {
			let mask = (crc & 1).wrapping_neg();
			crc = (crc >> 1) ^ (0xedb8_8320 & mask);
		}
	}
	!crc
}

// ---------------------------------------------------------------------------
// DEFLATE
// ---------------------------------------------------------------------------

/// Canonical Huffman table, built from code lengths (RFC 1951 3.2.2).
#[derive(Debug, Clone, Default)]
struct Huff {
	/// `counts[len]` = how many symbols have that length
	counts: [u16; 16],
	/// symbols sorted by (length, symbol), the order `puff`/zlib uses
	symbols: Vec<u16>,
}

impl Huff {
	fn build(lengths: &[u8]) -> Res<Huff> {
		let mut h = Huff { counts: [0; 16], symbols: vec![0; lengths.len().max(1)] };
		for &l in lengths {
			h.counts[l as usize] += 1;
		}
		h.counts[0] = 0;
		let mut offs = [0u16; 16];
		for i in 1..16 {
			offs[i] = offs[i - 1] + h.counts[i - 1];
		}
		for (sym, &l) in lengths.iter().enumerate() {
			if l != 0 {
				h.symbols[offs[l as usize] as usize] = sym as u16;
				offs[l as usize] += 1;
			}
		}
		Ok(h)
	}
}

struct Reader<'a> {
	src: &'a [u8],
	pos: usize,
	bitbuf: u32,
	nbits: u32,
}

impl<'a> Reader<'a> {
	fn new(src: &'a [u8]) -> Reader<'a> {
		Reader { src, pos: 0, bitbuf: 0, nbits: 0 }
	}

	fn bits(&mut self, n: u32) -> Res<u32> {
		while self.nbits < n {
			let b = match self.src.get(self.pos) {
				Some(b) => {
					self.pos += 1;
					*b
				}
				None => return Err(err("deflate stream ended unexpectedly".to_string())),
			};
			self.bitbuf |= (b as u32) << self.nbits;
			self.nbits += 8;
		}
		let v = self.bitbuf & ((1u32 << n) - 1);
		self.bitbuf >>= n;
		self.nbits -= n;
		Ok(v)
	}

	fn align(&mut self) {
		let drop_bits = self.nbits % 8;
		self.bitbuf >>= drop_bits;
		self.nbits -= drop_bits;
	}

	fn bytes(&mut self, n: usize) -> Res<&'a [u8]> {
		// a stored block starts on a byte boundary, so drop the partial byte first
		self.align();
		let byte_off = self.pos - (self.nbits / 8) as usize;
		if byte_off + n > self.src.len() {
			return Err(err("deflate stored block truncated".to_string()));
		}
		self.pos = byte_off + n;
		self.bitbuf = 0;
		self.nbits = 0;
		Ok(&self.src[byte_off..byte_off + n])
	}

	fn decode(&mut self, h: &Huff) -> Res<u16> {
		let mut code = 0i32;
		let mut first = 0i32;
		let mut index = 0i32;
		for len in 1..=15usize {
			code |= self.bits(1)? as i32;
			let count = h.counts[len] as i32;
			if code - first < count {
				return Ok(h.symbols[(index + (code - first)) as usize]);
			}
			index += count;
			first = (first + count) << 1;
			// the next bit of the stream becomes the low bit of `code`
			code <<= 1;
		}
		Err(err("deflate: invalid huffman code".to_string()))
	}
}

/// Decode a raw DEFLATE stream (`expect` is the uncompressed size from the zip
/// directory, used only to reserve capacity).
pub fn inflate_raw(src: &[u8], expect: u64, opts: &ZipOptions) -> Res<Vec<u8>> {
	let mut r = Reader::new(src);
	let mut out: Vec<u8> = Vec::with_capacity((expect.min(1024 * 1024)) as usize);
	const LENGTH_BASE: [u16; 29] = [
		3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195,
		227, 258,
	];
	const LENGTH_EXTRA: [u16; 29] = [
		0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
	];
	const DIST_BASE: [u16; 30] = [
		1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
		4097, 6145, 8193, 12289, 16385, 24577,
	];
	const DIST_EXTRA: [u16; 30] = [
		0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
	];
	let fixed_lit = Huff::build(&{
		let mut v = [0u8; 288];
		let mut i = 0;
		while i < 144 {
			v[i] = 8;
			i += 1;
		}
		while i < 256 {
			v[i] = 9;
			i += 1;
		}
		while i < 280 {
			v[i] = 7;
			i += 1;
		}
		while i < 288 {
			v[i] = 8;
			i += 1;
		}
		v
	})?;
	let fixed_dist = Huff::build(&[5u8; 30]);
	loop {
		let last = r.bits(1)? == 1;
		let kind = r.bits(2)?;
		match kind {
			0 => {
				let len_lo = r.bits(8)? as usize;
				let len_hi = r.bits(8)? as usize;
				let len = len_lo | (len_hi << 8);
				let n_lo = r.bits(8)? as usize;
				let n_hi = r.bits(8)? as usize;
				let nmask = n_lo | (n_hi << 8);
				if len ^ nmask != 0xffff {
					return Err(err(format!("deflate: stored block length mismatch ({} / {})", len, nmask)));
				}
				let data = r.bytes(len)?;
				out.extend_from_slice(data);
			}
			1 => {
				inflate_block(&mut r, &fixed_lit, &fixed_dist, &mut out, &LENGTH_BASE, &LENGTH_EXTRA, &DIST_BASE, &DIST_EXTRA)?;
			}
			2 => {
				let hlit = r.bits(5)? as usize + 257;
				let hdist = r.bits(5)? as usize + 1;
				let hclen = r.bits(4)? as usize + 4;
				if hlit > 288 || hdist > 30 {
					return Err(err(format!("deflate: too many codes ({} + {})", hlit, hdist)));
				}
				const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
				let mut code_lengths = [0u8; 19];
				for i in 0..hclen {
					code_lengths[ORDER[i]] = r.bits(3)? as u8;
				}
				let hcode = Huff::build(&code_lengths)?;
				let mut lengths = vec![0u8; hlit + hdist];
				let mut i = 0usize;
				while i < hlit + hdist {
					let sym = r.decode(&hcode)?;
					match sym {
						0..=15 => {
							lengths[i] = sym as u8;
							i += 1;
						}
						16 => {
							if i == 0 {
								return Err(err("deflate: repeat before first code length".to_string()));
							}
							let prev = lengths[i - 1];
							let rep = 3 + r.bits(2)? as usize;
							for _ in 0..rep {
								if i >= lengths.len() {
									return Err(err("deflate: code length overflow".to_string()));
								}
								lengths[i] = prev;
								i += 1;
							}
						}
						17 => {
							let rep = 3 + r.bits(3)? as usize;
							i += rep;
						}
						18 => {
							let rep = 11 + r.bits(7)? as usize;
							i += rep;
						}
						other => {
							return Err(err(format!("deflate: bad code length symbol {}", other)));
						}
					}
				}
				let lit = Huff::build(&lengths[..hlit])?;
				let dist = Huff::build(&lengths[hlit..])?;
				inflate_block(&mut r, &lit, &dist, &mut out, &LENGTH_BASE, &LENGTH_EXTRA, &DIST_BASE, &DIST_EXTRA)?;
			}
			_ => return Err(err("deflate: reserved block type 3".to_string())),
		}
		if out.len() as u64 > opts.max_entry_bytes {
			return Err(JadxError::new(
				ErrorKind::InputFormat,
				format!("zip entry inflates beyond the {} byte limit", opts.max_entry_bytes),
			));
		}
		if last {
			return Ok(out);
		}
	}
}

#[allow(clippy::too_many_arguments)]
fn inflate_block(
	r: &mut Reader<'_>,
	lit: &Huff,
	dist: &Huff,
	out: &mut Vec<u8>,
	length_base: &[u16; 29],
	length_extra: &[u16; 29],
	dist_base: &[u16; 30],
	dist_extra: &[u16; 30],
) -> Res<()> {
	loop {
		let sym = r.decode(lit)?;
		if sym < 256 {
			out.push(sym as u8);
		} else if sym == 256 {
			return Ok(());
		} else {
			let idx = sym as usize - 257;
			if idx >= length_base.len() {
				return Err(err(format!("deflate: bad length symbol {}", sym)));
			}
			let len = length_base[idx] as usize + r.bits(length_extra[idx] as u32)? as usize;
			let dsym = r.decode(dist)? as usize;
			if dsym >= dist_base.len() {
				return Err(err(format!("deflate: bad distance symbol {}", dsym)));
			}
			let back = dist_base[dsym] as usize + r.bits(dist_extra[dsym] as u32)? as usize;
			if back > out.len() {
				return Err(err(format!("deflate: distance {} beyond output ({})", back, out.len())));
			}
			let from = out.len() - back;
			// overlapping copies are legal: `a a a` expands byte by byte
			for k in 0..len {
				let b = out[from + k];
				out.push(b);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Build a minimal zip archive with one stored entry: enough to exercise the
	/// directory walk, and `deflate` entries are covered by [`deflate_round_trip`]
	/// through a hand-built stream.
	fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
		let mut out = Vec::new();
		let crc = crc32(data);
		// local file header
		out.extend_from_slice(&LOC_SIG.to_le_bytes());
		out.extend_from_slice(&[20, 0]); // version needed
		out.extend_from_slice(&[0, 0]); // flags
		out.extend_from_slice(&[0, 0]); // method = stored
		out.extend_from_slice(&[0, 0]); // time
		out.extend_from_slice(&[0, 0]); // date
		out.extend_from_slice(&crc.to_le_bytes());
		out.extend_from_slice(&(data.len() as u32).to_le_bytes());
		out.extend_from_slice(&(data.len() as u32).to_le_bytes());
		out.extend_from_slice(&(name.len() as u16).to_le_bytes());
		out.extend_from_slice(&0u16.to_le_bytes()); // extra
		out.extend_from_slice(name.as_bytes());
		out.extend_from_slice(data);
		let cen = out.len();
		// central directory
		out.extend_from_slice(&CEN_SIG.to_le_bytes());
		out.extend_from_slice(&[20, 0]);
		out.extend_from_slice(&[20, 0]);
		out.extend_from_slice(&[0, 0]);
		out.extend_from_slice(&[0, 0]);
		out.extend_from_slice(&[0, 0, 0, 0]);
		out.extend_from_slice(&crc.to_le_bytes());
		out.extend_from_slice(&(data.len() as u32).to_le_bytes());
		out.extend_from_slice(&(data.len() as u32).to_le_bytes());
		out.extend_from_slice(&(name.len() as u16).to_le_bytes());
		out.extend_from_slice(&0u16.to_le_bytes());
		out.extend_from_slice(&0u16.to_le_bytes()); // comment
		out.extend_from_slice(&[0, 0]); // disk
		out.extend_from_slice(&[0, 0]); // internal attrs
		out.extend_from_slice(&[0, 0, 0, 0]); // external attrs
		out.extend_from_slice(&(cen as u32).to_le_bytes());
		out.extend_from_slice(name.as_bytes());
		// eocd
		out.extend_from_slice(&EOCD_SIG.to_le_bytes());
		out.extend_from_slice(&[0, 0]);
		out.extend_from_slice(&[0, 0]);
		out.extend_from_slice(&[1, 0]);
		out.extend_from_slice(&[1, 0]);
		let cen_size = out.len() - cen;
		out.extend_from_slice(&(cen_size as u32).to_le_bytes());
		out.extend_from_slice(&(cen as u32).to_le_bytes());
		out.extend_from_slice(&0u16.to_le_bytes());
		out
	}

	#[test]
	fn a_stored_zip_is_read() {
		let bytes = stored_zip("pkg/T.class", b"hello jadx");
		let z = ZipArchive::open(&bytes).unwrap();
		assert_eq!(z.len(), 1);
		assert_eq!(z.entries()[0].name, "pkg/T.class");
		let data = z.read(0, &ZipOptions { verify_crc: true, ..Default::default() }).unwrap();
		assert_eq!(data, b"hello jadx");
		assert_eq!(z.find("pkg/T.class"), Some(0));
		let all = z.read_all_with_suffix(".class", &ZipOptions::default()).unwrap();
		assert_eq!(all.len(), 1);
		assert_eq!(all[0].1, b"hello jadx");
	}

	#[test]
	fn a_non_zip_buffer_is_rejected() {
		let e = ZipArchive::open(b"not a zip file at all, really").unwrap_err();
		assert!(e.message.contains("end of central directory"), "{}", e.message);
	}

	#[test]
	fn crc32_matches_the_reference_value() {
		assert_eq!(crc32(b""), 0);
		assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
		assert_eq!(crc32(b"the quick brown fox"), 0x47ee_6fee);
	}


	#[test]
	fn a_fixed_huffman_deflate_block_decodes() {
		// BFINAL=1, BTYPE=01 (fixed), then the 8-bit code for the literal 'a'
		// (48 + 97 = 145, written msb first), then end-of-block (7 zero bits).
		let data = [0x4Bu8, 0x06, 0x00];
		let out = inflate_raw(&data, 0, &ZipOptions::default()).unwrap();
		assert_eq!(out, b"a");
	}

	#[test]
	fn a_stored_deflate_block_decodes() {
		// BFINAL=1, BTYPE=00, LEN=3, NLEN=!3=0xfffc, then the raw bytes
		let mut data = vec![0b0000_0001u8, 0b0000_0000];
		data.extend_from_slice(&3u16.to_le_bytes());
		data.extend_from_slice(&(!3u16).to_le_bytes());
		data.extend_from_slice(b"xyz");
		let out = inflate_raw(&data, 0, &ZipOptions::default()).unwrap();
		assert_eq!(out, b"xyz");
	}

	#[test]
	fn a_corrupt_stored_block_is_rejected() {
		let mut data = vec![0b0000_0001u8, 0b0000_0000];
		data.extend_from_slice(&3u16.to_le_bytes());
		data.extend_from_slice(&3u16.to_le_bytes()); // NLEN must be !LEN
		data.extend_from_slice(b"xyz");
		let e = inflate_raw(&data, 0, &ZipOptions::default()).unwrap_err();
		assert!(e.message.contains("stored block length mismatch"), "{}", e.message);
	}

	#[test]
	fn an_empty_deflate_stream_yields_nothing() {
		// final fixed block (BFINAL=1, BTYPE=01) containing only the end-of-block
		// code, which is the 7-bit value 0 for the fixed literal tree
		let data = [0x03u8, 0x00];
		let out = inflate_raw(&data, 0, &ZipOptions::default()).unwrap();
		assert!(out.is_empty());
	}
}
