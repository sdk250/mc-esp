use std::fs;
use crate::error::{ ESPError, Result };


/// arm64 指针最高字节可能带标签（如 0xB4），用之前要去掉
pub const TAG_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;
const MODULE: &str = "Process";


/// maps 里的一行
#[derive(Debug, Clone)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub perms: String,
    pub offset: u64,
    pub path: String,
}

impl Region {
    pub fn readable(&self) -> bool {
        self.perms.starts_with('r')
    }

    pub fn writable(&self) -> bool {
        self.perms.as_bytes().get(1) == Some(&b'w')
    }
}


/// 遍历 /proc，按 cmdline 找到包名对应的 PID
pub fn find_pid(package: &str) -> Option<i32> {
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry
            .file_name()
            .to_string_lossy()
            .parse::<i32>()
            else { continue };
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline"))
            else { continue };

        // cmdline 以 \0 分隔，第一段就是进程名
        let name = cmdline.split(|&b| b == 0).next().unwrap_or_default();
        if name == package.as_bytes() {
            return Some(pid);
        }
    }
    None
}

/// 解析 maps
///
/// 每行格式：start-end perms offset dev inode [path]
pub fn read_maps(pid: i32) -> Result<Vec<Region>> {
    let text = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let mut regions = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(6, ' ');
        let (Some(range), Some(perms), Some(offset)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Some((start, end)) = range.split_once('-') else { continue };
        let path = parts.nth(2).unwrap_or("").trim().to_string(); // 跳过 dev、inode
        regions.push(Region {
            start: u64::from_str_radix(start, 16).unwrap_or(0),
            end: u64::from_str_radix(end, 16).unwrap_or(0),
            perms: perms.to_string(),
            offset: u64::from_str_radix(offset, 16).unwrap_or(0),
            path,
        });
    }
    Ok(regions)
}

/// 模块基址 = 路径以模块名结尾、且文件偏移为 0
pub fn module_base(regions: &[Region], module: &str) -> Option<u64> {
    regions
        .iter()
        .find(|r| r.offset == 0 && r.path.ends_with(module))
        .map(|r| r.start)
}

pub struct Process {
    pub pid: i32,
    pub base: u64,
    pub regions: Vec<Region>
}

impl Process {
    pub fn new(pid: i32) -> Self {
        Self { pid, base: 0x0, regions: Vec::new() }
    }

    /// 获取程序所有的内存段，以及定位基址
    pub fn init(&mut self) -> Result<()> {
        self.regions = read_maps(self.pid)?;
        match module_base(&self.regions, crate::GMODULE) {
            Some(v) => self.base = v,
            None => return Err(ESPError::Custom(MODULE, "Cannot to position module base.".into()))
        }

        Ok(())
    }

    /// 读一段内存，返回实际读到的字节数
    pub fn read_into(&self, addr: u64, buf: &mut [u8]) -> usize {
        let local = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
        let remote = libc::iovec { iov_base: addr as *mut libc::c_void, iov_len: buf.len() };

        // 一次系统调用把对方进程 addr 处的数据拷到 buf
        let n = unsafe { libc::process_vm_readv(
            self.pid, &local, 1,
            &remote, 1, 0
        ) };
        n.max(0) as usize
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64> {
        const BIT: usize = 0x8;
        let mut b = [0u8; BIT];
        match self.read_into(addr, &mut b) {
            0 => Err(ESPError::ZeroRead(MODULE)),
            BIT => Ok(u64::from_le_bytes(b)),
            v => Err(ESPError::NotFullyRead(MODULE, BIT, v))
        }
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32> {
        const BIT: usize = 0x4;
        let mut b = [0u8; BIT];
        match self.read_into(addr, &mut b) {
            0 => Err(ESPError::ZeroRead(MODULE)),
            BIT => Ok(u32::from_le_bytes(b)),
            v => Err(ESPError::NotFullyRead(MODULE, BIT, v))
        }
    }

    pub fn read_f32(&self, addr: u64) -> Result<f32> {
        self.read_u32(addr).map(f32::from_bits)
    }

    /// 读一个指针并去掉标签
    pub fn read_ptr(&self, addr: u64) -> Result<u64> {
        self.read_u64(addr).map(|v| v & TAG_MASK)
    }

    pub fn read_vec3(&self, addr: u64) -> Result<[f32; 3]> {
        const BIT: usize = 0xC;
        let mut b = [0u8; BIT];
        match self.read_into(addr, &mut b) {
            0 => return Err(ESPError::ZeroRead(MODULE)),
            BIT => { },
            v => return Err(ESPError::NotFullyRead(MODULE, BIT, v))
        }
        let f = |i: usize| f32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Ok([f(0), f(4), f(8)])
    }

    pub fn read_str(&self, addr: u64, n: usize) -> Result<String> {
        let mut buf = vec![0u8; n];
        let k = self.read_into(addr, &mut buf);
        if k == 0 {
            return Err(ESPError::ZeroRead(MODULE));
        }
        if let Some(end) = buf[..k].iter().position(| v | *v == 0) {
            Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
        } else {
            Err(ESPError::Custom(MODULE, "Failed to find terminated char.".into()))
        }
    }
}
