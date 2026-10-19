//! Valve's KeyValues3 in the binary form CS2 compiles into a material's
//! DATA block (format version 5): a 120-byte header, two LZ4 parts, and
//! lanes of 1-, 2-, 4- and 8-byte values that the node types draw from in
//! order. Part 1 carries the string table and the "auxiliary" values
//! (vectors), part 2 the object member counts, the scalars, the member name
//! indices and the type stream.

use crate::lz4;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
    /// A binary blob, by length; the bytes themselves are not kept.
    Blob(usize),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members
                .iter()
                .rev()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Double(value) => Some(*value),
            Value::Int(value) => Some(*value as f64),
            Value::Bool(value) => Some(f64::from(u8::from(*value))),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(value) => Some(*value),
            Value::Double(value) => Some(*value as i64),
            Value::Bool(value) => Some(i64::from(*value)),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

fn align(at: usize, to: usize) -> usize {
    at.div_ceil(to) * to
}

/// One buffer's four lanes, each read front to back.
struct Lanes<'a> {
    lanes: [&'a [u8]; 4],
    at: [usize; 4],
}

impl<'a> Lanes<'a> {
    /// Carves the lanes out of `buffer` from `start`, given how many
    /// values each holds; returns them with the offset after the last lane.
    fn carve(buffer: &'a [u8], start: usize, counts: [usize; 4]) -> Result<(Self, usize), String> {
        let mut lanes: [&[u8]; 4] = [&[]; 4];
        let mut at = start;
        for (lane, width) in [1usize, 2, 4, 8].into_iter().enumerate() {
            // An empty lane takes no room, not even its alignment padding.
            if counts[lane] == 0 {
                continue;
            }
            at = align(at, width);
            let bytes = counts[lane] * width;
            lanes[lane] = buffer
                .get(at..at + bytes)
                .ok_or_else(|| format!("kv3: lane {width} runs past its buffer"))?;
            at += bytes;
        }
        Ok((Self { lanes, at: [0; 4] }, at))
    }

    fn take(&mut self, lane: usize, bytes: usize) -> Result<&'a [u8], String> {
        let start = self.at[lane];
        let out = self.lanes[lane]
            .get(start..start + bytes)
            .ok_or_else(|| format!("kv3: lane {} exhausted", [1, 2, 4, 8][lane]))?;
        self.at[lane] += bytes;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(0, 1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(1, 2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(2, 4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(3, 8)?.try_into().unwrap()))
    }

    fn string_z(&mut self) -> Result<String, String> {
        let lane = self.lanes[0];
        let start = self.at[0];
        let end = lane[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|n| start + n)
            .ok_or("kv3: unterminated string")?;
        self.at[0] = end + 1;
        Ok(String::from_utf8_lossy(&lane[start..end]).into_owned())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Main,
    Aux,
}

struct Reader<'a> {
    main: Lanes<'a>,
    aux: Lanes<'a>,
    strings: Vec<String>,
    types: &'a [u8],
    type_at: usize,
    object_lengths: Vec<u32>,
    object_at: usize,
    blob_lengths: Vec<u32>,
    blob_at: usize,
    /// Versions before 5 read each object's member count from the 4-byte
    /// lane instead of a table.
    objects_inline: bool,
}

impl<'a> Reader<'a> {
    fn lane(&mut self, which: Which) -> &mut Lanes<'a> {
        match which {
            Which::Main => &mut self.main,
            Which::Aux if self.objects_inline => &mut self.main,
            Which::Aux => &mut self.aux,
        }
    }

    fn read_type(&mut self) -> Result<u8, String> {
        let byte = *self.types.get(self.type_at).ok_or("kv3: type stream exhausted")?;
        self.type_at += 1;
        if byte & 0x80 != 0 {
            self.type_at += 1; // the flag byte (resource, panorama, ...)
        }
        if byte & 0x40 != 0 {
            self.type_at += 1;
        }
        Ok(byte & 0x3F)
    }

    fn string(&self, index: i32) -> String {
        usize::try_from(index)
            .ok()
            .and_then(|i| self.strings.get(i))
            .cloned()
            .unwrap_or_default()
    }

    fn read_value(&mut self, kind: u8, which: Which) -> Result<Value, String> {
        Ok(match kind {
            1 => Value::Null,
            2 => Value::Bool(self.lane(which).u8()? != 0),
            3 => Value::Int(self.lane(which).u64()? as i64),
            4 => Value::Int(self.lane(which).u64()? as i64),
            5 => Value::Double(f64::from_bits(self.lane(which).u64()?)),
            6 => {
                let index = self.lane(which).u32()? as i32;
                Value::Str(self.string(index))
            }
            7 => {
                let length = self.blob_lengths.get(self.blob_at).copied().unwrap_or(0);
                self.blob_at += 1;
                Value::Blob(length as usize)
            }
            8 => {
                let count = self.main.u32()? as usize;
                let mut items = Vec::with_capacity(count.min(1 << 16));
                for _ in 0..count {
                    let kind = self.read_type()?;
                    items.push(self.read_value(kind, which)?);
                }
                Value::Array(items)
            }
            9 => {
                let count = if self.objects_inline {
                    self.main.u32()? as usize
                } else {
                    let count = *self
                        .object_lengths
                        .get(self.object_at)
                        .ok_or("kv3: object count table exhausted")? as usize;
                    self.object_at += 1;
                    count
                };
                let mut members = Vec::with_capacity(count.min(1 << 16));
                for _ in 0..count {
                    let kind = self.read_type()?;
                    let name = self.main.u32()? as i32;
                    let name = self.string(name);
                    members.push((name, self.read_value(kind, which)?));
                }
                Value::Object(members)
            }
            10 => {
                let count = self.main.u32()? as usize;
                let element = self.read_type()?;
                let mut items = Vec::with_capacity(count.min(1 << 16));
                for _ in 0..count {
                    items.push(self.read_value(element, which)?);
                }
                Value::Array(items)
            }
            11 => Value::Int(i64::from(self.lane(which).u32()? as i32)),
            12 => Value::Int(i64::from(self.lane(which).u32()?)),
            13 => Value::Bool(true),
            14 => Value::Bool(false),
            15 => Value::Int(0),
            16 => Value::Int(1),
            17 => Value::Double(0.0),
            18 => Value::Double(1.0),
            19 => Value::Double(f64::from(f32::from_bits(self.lane(which).u32()?))),
            20 => Value::Int(i64::from(self.lane(which).u16()? as i16)),
            21 => Value::Int(i64::from(self.lane(which).u16()?)),
            23 => Value::Int(i64::from(self.lane(which).u8()? as i8)),
            24 | 25 => {
                let count = usize::from(self.main.u8()?);
                let element = self.read_type()?;
                let from = if kind == 25 { Which::Aux } else { which };
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.read_value(element, from)?);
                }
                Value::Array(items)
            }
            other => return Err(format!("kv3: node type {other} is not known")),
        })
    }
}

/// Parses a resource's DATA block holding binary KV3 version 5.
pub fn parse(data: &[u8]) -> Result<Value, String> {
    if data.len() < 120 {
        return Err("kv3: block too short".into());
    }
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    let u16_at = |at: usize| usize::from(u16::from_le_bytes([data[at], data[at + 1]]));
    let magic = u32_at(0);
    if magic >> 8 != 0x4B_5633 {
        return Err(format!("kv3: magic {magic:#x} is not KV3"));
    }
    let version = magic & 0xFF;
    if version == 3 || version == 4 {
        return parse_single_buffer(data, version);
    }
    if version != 5 {
        return Err(format!("kv3: version {version} (only 3 to 5 are read)"));
    }
    let compression = u32_at(20);
    let count_bytes1 = u32_at(28);
    let count_bytes4 = u32_at(32);
    let count_bytes8 = u32_at(36);
    let count_types = u32_at(40);
    let _count_objects = u16_at(44);
    let _count_arrays = u16_at(46);
    let uncompressed_total = u32_at(48);
    let count_blocks = u32_at(56);
    let count_bytes2 = u32_at(64);
    let uncompressed1 = u32_at(72);
    let compressed1 = u32_at(76);
    let uncompressed2 = u32_at(80);
    let compressed2 = u32_at(84);
    let count_bytes1_main = u32_at(88);
    let count_bytes2_main = u32_at(92);
    let count_bytes4_main = u32_at(96);
    let count_bytes8_main = u32_at(100);
    let count_objects_main = u32_at(108);
    if uncompressed_total != uncompressed1 + uncompressed2 {
        return Err("kv3: part sizes do not add up".into());
    }
    let body = &data[120..];
    let (part1, part2): (Vec<u8>, Vec<u8>) = match compression {
        0 => (
            body.get(..uncompressed1).ok_or("kv3: part 1 truncated")?.to_vec(),
            body.get(uncompressed1..uncompressed1 + uncompressed2)
                .ok_or("kv3: part 2 truncated")?
                .to_vec(),
        ),
        1 => (
            lz4::decompress(body.get(..compressed1).ok_or("kv3: part 1 truncated")?, uncompressed1)?,
            lz4::decompress(
                body.get(compressed1..compressed1 + compressed2)
                    .ok_or("kv3: part 2 truncated")?,
                uncompressed2,
            )?,
        ),
        other => return Err(format!("kv3: compression method {other} is not supported")),
    };

    // Part 1: the lanes the auxiliary reads draw from, strings first.
    let (mut aux, _) = Lanes::carve(&part1, 0, [count_bytes1, count_bytes2, count_bytes4, count_bytes8])?;
    let string_count = aux.u32()? as usize;
    let mut strings = Vec::with_capacity(string_count.min(1 << 16));
    for _ in 0..string_count {
        strings.push(aux.string_z()?);
    }

    // Part 2: object member counts, the main lanes, then the type stream.
    let objects_end = count_objects_main * 4;
    let object_lengths: Vec<u32> = part2
        .get(..objects_end)
        .ok_or("kv3: object count table truncated")?
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let (main, lanes_end) = Lanes::carve(
        &part2,
        objects_end,
        [count_bytes1_main, count_bytes2_main, count_bytes4_main, count_bytes8_main],
    )?;
    let trailer = if count_blocks == 0 { 4 } else { 0 };
    let types_start = part2
        .len()
        .checked_sub(trailer + count_blocks * 4 + count_types)
        .filter(|&start| start >= lanes_end)
        .unwrap_or(lanes_end);
    let types = part2
        .get(types_start..types_start + count_types)
        .ok_or("kv3: type stream truncated")?;
    let blob_lengths: Vec<u32> = part2
        .get(types_start + count_types..types_start + count_types + count_blocks * 4)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();

    let mut reader = Reader {
        main,
        aux,
        strings,
        types,
        type_at: 0,
        object_lengths,
        object_at: 0,
        blob_lengths,
        blob_at: 0,
        objects_inline: false,
    };
    let root = reader.read_type()?;
    reader.read_value(root, Which::Main)
}

/// Versions 3 and 4 (models): one buffer holding the lanes, then the
/// strings, the type stream and any blob lengths; object member counts
/// are read from the 4-byte lane as they come.
fn parse_single_buffer(data: &[u8], version: usize) -> Result<Value, String> {
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    let compression = u32_at(20);
    let count_bytes1 = u32_at(28);
    let count_bytes4 = u32_at(32);
    let count_bytes8 = u32_at(36);
    let strings_and_types = u32_at(40);
    let uncompressed = u32_at(48);
    let compressed = u32_at(52);
    let count_blocks = u32_at(56);
    let (count_bytes2, header) = if version >= 4 {
        if data.len() < 72 {
            return Err("kv3: header truncated".into());
        }
        (u32_at(64), 72)
    } else {
        (0, 64)
    };
    let body = data.get(header..).ok_or("kv3: header truncated")?;
    let buffer: Vec<u8> = match compression {
        0 => body.get(..uncompressed).ok_or("kv3: buffer truncated")?.to_vec(),
        1 => lz4::decompress(body.get(..compressed).ok_or("kv3: buffer truncated")?, uncompressed)?,
        other => return Err(format!("kv3: compression method {other} is not supported")),
    };
    let (mut lanes, lanes_end) =
        Lanes::carve(&buffer, 0, [count_bytes1, count_bytes2, count_bytes4, count_bytes8])?;
    let string_count = lanes.u32()? as usize;
    let mut at = lanes_end;
    let mut strings = Vec::with_capacity(string_count.min(1 << 16));
    for _ in 0..string_count {
        let end = buffer[at..]
            .iter()
            .position(|&b| b == 0)
            .map(|n| at + n)
            .ok_or("kv3: unterminated string")?;
        strings.push(String::from_utf8_lossy(&buffer[at..end]).into_owned());
        at = end + 1;
    }
    let types_len = strings_and_types.saturating_sub(at - lanes_end);
    let types = buffer.get(at..at + types_len).ok_or("kv3: type stream truncated")?;
    at += types_len;
    let blob_lengths: Vec<u32> = buffer
        .get(at..at + count_blocks * 4)
        .unwrap_or(&[])
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    // One buffer serves as both lanes: the aux reads share the cursors.
    let main = Lanes {
        lanes: lanes.lanes,
        at: lanes.at,
    };
    let mut reader = Reader {
        main,
        aux: Lanes {
            lanes: [&[]; 4],
            at: [0; 4],
        },
        strings,
        types,
        type_at: 0,
        object_lengths: Vec::new(),
        object_at: 0,
        blob_lengths,
        blob_at: 0,
        objects_inline: true,
    };
    let root = reader.read_type()?;
    reader.read_value(root, Which::Main)
}
