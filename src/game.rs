use crate::{
    error::{ ESPError, Result },
    process::{ Process, TAG_MASK, Region }
};

use tracing::info;


const MODULE: &str = "Game_Module";
pub const STORAGE_ID: u32 = 0x85B93800;
pub const ROTATION_ID: u32 = 0x75DF36B7;

pub struct Actor {
    pub addr: u64,
    pub rtti: String
}

/// 遍历 ActorOwnerComponent 的存储池，返回所有活着的 Actor 地址（跳过墓碑）
///
/// TODO: 只读了第 0 页；超过 128 个实体时要按页表继续读
pub fn list_actors(proc: &Process, entry_addr: u64) -> Result<Vec<Actor>> {
    let slot_count = (proc.read_ptr(entry_addr + 0x28)? - proc.read_ptr(entry_addr + 0x20)?) / 4;
    let page0 = proc.read_ptr(proc.read_ptr(entry_addr + 0x50)?)?;
    let mut ret = Vec::new();
    for n in 0..slot_count {
        let actor_addr = proc.read_ptr(page0 + n * 0x8)?;
        if actor_addr != 0x0 {
            ret.push(Actor { addr: actor_addr, rtti: rtti_name(proc, actor_addr)? });
        }
    }

    Ok(ret)
}

/// 通过任意一个 actor 的 vptr 地址反向跟踪到 Rotation 组件的位置
///
/// 再通过 eid 定位具体对象的 pitch 起始地址并返回
pub fn fetch_rotation_addr(proc: &Process, client_actor_addr: u64, eid: u32) -> Result<u64> {
    let rotation_addr = fetch_component_addr(
        &proc, client_actor_addr,
        ROTATION_ID
    )?;
    let begin = proc.read_ptr(rotation_addr + 0x20)?;
    let end = proc.read_ptr(rotation_addr + 0x28)?;
    let mut index = None;
    for (i, j) in (begin..end).step_by(4).enumerate() {
        if proc.read_u32(j)? == eid {
            index = Some(i);
            break;
        }
    }
    if let Some(i) = index {
        return Ok(proc.read_ptr(proc.read_ptr(
            rotation_addr + 0x50
        )?)? + (i * 0x10) as u64);
    } else {
        return Err(ESPError::Custom(MODULE, format!("Not found the eid: {}", eid).into()));
    }
}

/// 通过任意一个 actor 的 vptr 地址反向跟踪到 component_id 组件的位置
pub fn fetch_component_addr(proc: &Process, client_actor_addr: u64, component_id: u32) -> Result<u64> {
    let register_addr = proc.read_ptr(client_actor_addr + 0x10)?;
    let register_begin = proc.read_ptr(register_addr + 0x50)?;
    let register_end = proc.read_ptr(register_addr + 0x58)?;
    let mut register_item = [0u8; 0x20];
    let mut register_is_found = false;
    for index in (register_begin..register_end).step_by(0x20) {
        if proc.read_into(index, &mut register_item) != 0x20 {
            return Err(ESPError::ZeroRead(MODULE));
        }
        if u32::from_le_bytes(register_item[0x8..0x8 + 0x4].try_into().unwrap()) == component_id {
            register_is_found = true;
            break;
        }
    }
    if ! register_is_found { return Err(ESPError::ZeroRead(MODULE)); }
    Ok(u64::from_le_bytes(register_item[0x10..0x18].try_into().unwrap()) & TAG_MASK)
}

/// 返回指定对象的 RTTI
fn rtti_name(proc: &Process, obj_addr: u64) -> Result<String> {
    let typeinfo = proc.read_ptr(proc.read_ptr(obj_addr)? - 0x8)?;
    proc.read_str(proc.read_ptr(typeinfo + 0x8)?, 0xFF)
}

/// 找出能把 Actor 恰好对半分成两组的字段（同组值相等、两组值不同），不预设谁属于哪组
///
/// 每个 Actor 只整体读一次，保证比较的是同一时刻的快照
#[allow(dead_code)]
pub fn diff_actors(proc: &Process, actors: &[u64]) -> Result<()> {
    const ACTOR_SIZE: usize = 0x448;
    if actors.len() < 2 || actors.len() % 2 != 0 {
        return Err(ESPError::Custom(MODULE, format!("Need an even number of actors, got {}", actors.len()).into()));
    }

    let mut snaps = Vec::with_capacity(actors.len());
    for &addr in actors {
        let mut buf = vec![0u8; ACTOR_SIZE];
        if proc.read_into(addr, &mut buf) != ACTOR_SIZE {
            return Err(ESPError::Custom(MODULE, format!("Failed to read actor 0x{:X}", addr).into()));
        }
        snaps.push(buf);
    }

    let half = actors.len() / 2;
    for off in (0..ACTOR_SIZE).step_by(8) {
        let vals: Vec<u64> = snaps
            .iter()
            .map(|s| u64::from_le_bytes(s[off..off + 8].try_into().unwrap()))
            .collect();

        // 以第 0 个 Actor 的值为 0 组，其余必须要么等于它、要么等于同一个另一值
        let v0 = vals[0];
        let Some(&v1) = vals.iter().find(|&&v| v != v0) else { continue; };
        if vals.iter().any(|&v| v != v0 && v != v1) { continue; }
        if vals.iter().filter(|&&v| v == v0).count() != half { continue; }

        // 分组签名，例如 "000111" 表示前三个一组、后三个一组；真正的分组字段签名应该一致
        let sig: String = vals.iter().map(|&v| if v == v0 { '0' } else { '1' }).collect();
        info!("offset: 0x{:03X}  group: {}  0: {:#018X}  1: {:#018X}", off, sig, v0, v1);
    }

    Ok(())
}

pub fn scan_actors(proc: &Process, vptr: u64) -> Result<Vec<u64>> {
    fn is_heap(r: &&Region) -> bool {
        if ! r.readable() || ! r.writable() {
            return false;
        }
        r.path.contains("scudo") || r.path.contains("malloc") || r.path == "[heap]"
    }

    const CHUNK: usize = 4 << 20;
    #[allow(non_snake_case)]
    let PAGE_SIZE = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 };

    let mut buf = vec![0u8; CHUNK];
    let mut found = Vec::new();
    for r in proc.regions.iter().filter(is_heap) {
        let mut curr = r.start;
        while curr < r.end {
            let len = CHUNK.min((r.end - curr) as usize);
            let n = proc.read_into(curr, &mut buf[..len]);
            for i in (0..n.saturating_sub(7)).step_by(8) {
                if u64::from_le_bytes(buf[i..i + 8].try_into().unwrap()) == vptr {
                    found.push(curr + i as u64);
                }
            }
            curr += if n == 0 {
                PAGE_SIZE
            } else {
                n as u64
            };
        }
    }

    Ok(found)
}
