//! The bounded MOS boot record inside U-Boot's redundant MMC environment.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

pub const ENV_SIZE: usize = 65536;
/// Compiled board geometry; disk contents never choose writable offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitLayout {
    Cx3576,
    S905x5m,
}

impl FitLayout {
    pub fn for_board(board: &str) -> Result<Self> {
        match board {
            "cx3576" => Ok(Self::Cx3576),
            "s905x5m" => Ok(Self::S905x5m),
            _ => anyhow::bail!("unsupported FIT layout"),
        }
    }

    /// Relative to FIRMWARE, which starts at absolute disk sector 64.
    pub const fn offsets(self) -> [u64; 2] {
        let mib = match self {
            Self::Cx3576 => [16, 17],
            Self::S905x5m => [120, 124],
        };
        [mib[0] * 1048576 - 32768, mib[1] * 1048576 - 32768]
    }

    pub const fn sectors(self) -> u64 {
        match self {
            Self::Cx3576 => 36800,
            Self::S905x5m => 262080,
        }
    }
}
const MAX_GENERATION: u64 = 9_007_199_254_740_991;
const KEY: &[u8] = b"mos_entries=";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub kernel_id: String,
    pub generation: u64,
    pub tries_left: Option<u8>,
}

pub fn parse_records(text: &str) -> Result<Vec<Record>> {
    ensure!(text.len() <= 512, "boot records exceed bound");
    let mut records = Vec::new();
    for row in text
        .strip_prefix("v1|")
        .context("invalid boot record version")?
        .split(';')
    {
        let fields: Vec<_> = row.split(',').collect();
        ensure!(
            fields.len() == 4 && records.len() < 2,
            "invalid boot record count or fields"
        );
        crate::deployments::valid_id(fields[0])?;
        crate::deployments::valid_id(fields[1])?;
        let generation: u64 = fields[2].parse()?;
        ensure!(
            generation > 0 && generation <= MAX_GENERATION && generation.to_string() == fields[2],
            "invalid boot generation"
        );
        ensure!(
            records
                .iter()
                .all(|r: &Record| r.id != fields[0] && r.generation > generation),
            "ambiguous boot record order"
        );
        let tries_left = match fields[3] {
            "-" => None,
            "0" => Some(0),
            "1" => Some(1),
            "2" => Some(2),
            "3" => Some(3),
            _ => anyhow::bail!("invalid boot attempt counter"),
        };
        records.push(Record {
            id: fields[0].into(),
            kernel_id: fields[1].into(),
            generation,
            tries_left,
        });
    }
    Ok(records)
}

pub fn render_records(records: &[Record]) -> Result<String> {
    let text = format!(
        "v1|{}",
        records
            .iter()
            .map(|r| format!(
                "{},{},{},{}",
                r.id,
                r.kernel_id,
                r.generation,
                r.tries_left.map_or_else(|| "-".into(), |v| v.to_string())
            ))
            .collect::<Vec<_>>()
            .join(";")
    );
    parse_records(&text)?;
    Ok(text)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

pub fn encode(records: &[Record], flag: u8) -> Result<Vec<u8>> {
    let value = render_records(records)?;
    let mut bytes = vec![0; ENV_SIZE];
    bytes[4] = flag;
    bytes[5..5 + KEY.len()].copy_from_slice(KEY);
    bytes[5 + KEY.len()..5 + KEY.len() + value.len()].copy_from_slice(value.as_bytes());
    let crc = crc32(&bytes[5..]);
    bytes[..4].copy_from_slice(&crc.to_le_bytes());
    Ok(bytes)
}

fn valid_crc(bytes: &[u8]) -> bool {
    bytes.len() == ENV_SIZE && bytes[..4] == crc32(&bytes[5..]).to_le_bytes()
}

fn decode(bytes: &[u8]) -> Result<Vec<Record>> {
    let data = &bytes[5..];
    ensure!(
        data.starts_with(KEY),
        "boot records absent from environment"
    );
    let end = data
        .iter()
        .position(|byte| *byte == 0)
        .context("unterminated boot environment")?;
    ensure!(
        end + 1 < data.len() && data[end..].iter().all(|byte| *byte == 0),
        "unexpected boot environment data"
    );
    parse_records(std::str::from_utf8(&data[KEY.len()..end])?)
}

#[derive(Debug)]
pub struct Environment {
    pub records: Vec<Record>,
    pub slot: usize,
    pub flag: u8,
    layout: FitLayout,
}

impl Environment {
    pub fn load(region: &Path, layout: FitLayout) -> Result<Self> {
        let mut file = File::open(region)?;
        let mut copies = [vec![0; ENV_SIZE], vec![0; ENV_SIZE]];
        let mut valid = [false; 2];
        for i in 0..2 {
            if file.seek(SeekFrom::Start(layout.offsets()[i])).is_ok()
                && file.read_exact(&mut copies[i]).is_ok()
            {
                valid[i] = valid_crc(&copies[i]);
            }
        }
        // Match env_check_redund() in the pinned U-Boot source, including wrap.
        let slot = match valid {
            [true, false] => 0,
            [false, true] => 1,
            [false, false] => anyhow::bail!("no valid redundant boot environment"),
            [true, true] => {
                let (a, b) = (copies[0][4], copies[1][4]);
                if (a == 255 && b == 0) || (b > a && !(b == 255 && a == 0)) {
                    1
                } else {
                    0
                }
            }
        };
        Ok(Self {
            records: decode(&copies[slot])?,
            slot,
            flag: copies[slot][4],
            layout,
        })
    }

    /// The caller holds DATA's deployment transaction lock. Write the inactive
    /// environment, flush the block device, then require identical read-back.
    pub fn save(&self, region: &Path) -> Result<()> {
        ensure!(self.slot < 2, "invalid active environment slot");
        let slot = 1 - self.slot;
        let flag = self.flag.wrapping_add(1);
        let bytes = encode(&self.records, flag)?;
        let mut file = OpenOptions::new().write(true).open(region)?;
        file.seek(SeekFrom::Start(self.layout.offsets()[slot]))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        let saved = Self::load(region, self.layout)?;
        ensure!(
            saved.slot == slot && saved.flag == flag && saved.records == self.records,
            "boot environment read-back mismatch"
        );
        Ok(())
    }
}
