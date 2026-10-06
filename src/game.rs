use crate::{
    kinds::{ self, Kind },
    error::{ ESPError, Result },
    process::{ Process, TAG_MASK, Region }
};

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;


const MODULE: &str = "Game_Module";
pub const STORAGE_ID: u32 = 0x85B93800;
pub const ROTATION_ID: u32 = 0x75DF36B7;
/// FNV1a32("AABBShapeComponent")：碰撞箱组件
pub const AABB_SHAPE_ID: u32 = 0xBAC1B3CF;
/// LocalPlayer（RTTI 11LocalPlayer）的 vtable 相对基址的偏移。只对当前游戏版本有效
pub const LOCAL_PLAYER_VPTR_OFFSET: u64 = 0x12D08F98;

#[derive(Debug, Clone)]
pub struct Actor {
    pub addr: u64,
    /// RTTI 类名，例如 "6Animal"。同一个类的所有实体共用一份字符串
    pub rtti: Arc<str>,
    /// 实体类型名，例如 "minecraft:cow"；读不到时为空串
    pub type_name: Arc<str>,
    /// 按类型名得到的分类，决定框的颜色、画不画
    pub kind: Kind,
    /// 碰撞箱的宽、高（格），来自 AABBShapeComponent；读不到时用 DEFAULT_SIZE
    pub size: [f32; 2],
    /// StateVector 的 pos 比碰撞箱底部高多少。生物的 pos 在脚底（≈0），玩家的 pos 在眼睛（≈1.62），
    /// 画框时用 pos.y - foot_offset 当作脚底
    pub foot_offset: f32,
    /// [Actor+0x208]：StateVectorComponent 的地址（+0x00 pos，+0x0C posPrev）。
    /// 在实体的整个生命周期里不变，所以刷新列表时读一次，渲染时直接用
    pub state_vector: u64,
}

/// Actor 里指向 StateVectorComponent 的指针的偏移
pub const STATE_VECTOR_PTR_OFFSET: u64 = 0x208;

/// Actor 里实体类型名（HashedString：u64 哈希 + std::string）的偏移
const TYPE_NAME_OFFSET: u64 = 0x228;

/// 名字缓存：同一个类的对象 vptr 相同、同一种实体的类型名哈希相同，名字都只需要解析一次
#[derive(Default)]
pub struct NameCache {
    /// vptr → RTTI 类名
    rtti: HashMap<u64, Arc<str>>,
    /// 类型名哈希 → 类型名
    types: HashMap<u64, Arc<str>>,
}

/// 读 Actor +0x228 的 HashedString，返回类型名（例如 "minecraft:cow"）
///
/// 布局：+0x00 u64 哈希，+0x08 std::string（NDK libc++，24 字节）：
///   - 短字符串（≤ 22 字节）：第 0 字节最低位为 0，长度 = 第 0 字节 >> 1，字符紧跟其后；
///   - 长字符串：第 0 个 u64 最低位为 1（容量），+0x08 长度，+0x10 指向字符的指针。
fn read_type_name(proc: &Process, actor: u64, cache: &mut HashMap<u64, Arc<str>>) -> Option<Arc<str>> {
    let mut b = [0u8; 32];
    if proc.read_into(actor + TYPE_NAME_OFFSET, &mut b) != b.len() { return None; }
    let hash = u64::from_le_bytes(b[..8].try_into().unwrap());
    if hash == 0 { return None; }
    if let Some(name) = cache.get(&hash) { return Some(name.clone()); }

    let s = &b[8..];
    let bytes = if s[0] & 1 == 0 {
        let len = (s[0] >> 1) as usize;
        if len > 22 { return None; }
        s[1..1 + len].to_vec()
    } else {
        let len = u64::from_le_bytes(s[8..16].try_into().unwrap()) as usize;
        let ptr = u64::from_le_bytes(s[16..24].try_into().unwrap()) & TAG_MASK;
        if len == 0 || len > 128 { return None; }
        let mut buf = vec![0u8; len];
        if proc.read_into(ptr, &mut buf) != len { return None; }
        buf
    };
    let name: Arc<str> = String::from_utf8(bytes).ok()?.into();
    cache.insert(hash, name.clone());
    Some(name)
}

/// 组件存储池每页的槽位数。这个游戏把 EnTT 的页大小配置成了 128 个元素：
/// ActorOwnerComponent（8 字节）一页 1 KB，AABBShapeComponent（32 字节）一页 4 KB
const SLOTS_PER_PAGE: usize = 128;

/// AABBShapeComponent 的布局（32 字节）：min(xyz) max(xyz) 宽 高，全是 f32
const AABB_SHAPE_SIZE: u64 = 0x20;
/// 读不到碰撞箱时用的尺寸（玩家大小）
pub const DEFAULT_SIZE: [f32; 2] = [0.6, 1.8];

/// Actor 里 EntityId 的偏移（EntityContext：+0x08 EntityRegistry、+0x10 entt registry、+0x18 EntityId）
const ENTITY_ID_OFFSET: u64 = 0x18;

/// 某个组件存储池的一份快照：EntityId → 槽位下标，以及每一页的地址。按 EntityId 找这个实体的那一份组件
struct ComponentIndex {
    slots: HashMap<u32, usize>,
    pages: Vec<u64>,
}

impl ComponentIndex {
    /// 读存储池的 packed 数组（[S+0x20..S+0x28]，每个元素是 u32 EntityId）和页表（[S+0x50..S+0x58]）
    fn read(proc: &Process, storage: u64) -> Result<Self> {
        let read_vec = |begin_off: u64, elem: usize| -> Result<Vec<u8>> {
            let begin = proc.read_ptr(storage + begin_off)?;
            let end = proc.read_ptr(storage + begin_off + 8)?;
            let len = (end.saturating_sub(begin) as usize / elem) * elem;
            let mut buf = vec![0u8; len];
            if proc.read_into(begin, &mut buf) != len {
                return Err(ESPError::NotFullyRead(MODULE, len, 0));
            }
            Ok(buf)
        };
        let slots = read_vec(0x20, 4)?
            .chunks_exact(4)
            .enumerate()
            .map(|(i, c)| (u32::from_le_bytes(c.try_into().unwrap()), i))
            .collect();
        let pages = read_vec(0x50, 8)?
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()) & TAG_MASK)
            .collect();
        Ok(Self { slots, pages })
    }

    /// 这个实体的那一份组件的地址；元素大小为 `elem_size` 字节
    fn element(&self, eid: u32, elem_size: u64) -> Option<u64> {
        let i = *self.slots.get(&eid)?;
        let page = *self.pages.get(i / SLOTS_PER_PAGE)?;
        (page != 0).then(|| page + (i % SLOTS_PER_PAGE) as u64 * elem_size)
    }
}

/// 遍历 ActorOwnerComponent 的存储池，返回所有活着的 Actor（跳过墓碑）
///
/// 开销尽量小，可以频繁调用：
///   - 每一页用一次系统调用整页读出，而不是逐个槽位读；
///   - 类名按 vptr 缓存，命中时每个实体只需要读 1 次 vptr；
///   - 单个实体读取失败（比如它恰好在这时被销毁）只跳过它，不影响整个列表。
///
/// `aabb_storage` 是 AABBShapeComponent 的存储池，用来读每个实体的碰撞箱尺寸；为 None 时一律用 DEFAULT_SIZE
pub fn list_actors(proc: &Process, entry_addr: u64, aabb_storage: Option<u64>, names: &mut NameCache) -> Result<Vec<Actor>> {
    let aabb = aabb_storage.and_then(|s| ComponentIndex::read(proc, s).ok());
    let slot_count = ((proc.read_ptr(entry_addr + 0x28)? - proc.read_ptr(entry_addr + 0x20)?) / 4) as usize;
    let pages_begin = proc.read_ptr(entry_addr + 0x50)?;
    let page_count = slot_count.div_ceil(SLOTS_PER_PAGE);

    // 页表：一次读出所有页的指针
    let mut page_table = vec![0u8; page_count * 8];
    if proc.read_into(pages_begin, &mut page_table) != page_table.len() {
        return Err(ESPError::NotFullyRead(MODULE, page_table.len(), 0));
    }

    let mut ret = Vec::new();
    let mut page_buf = vec![0u8; SLOTS_PER_PAGE * 8];
    for (page_index, raw) in page_table.chunks_exact(8).enumerate() {
        let page = u64::from_le_bytes(raw.try_into().unwrap()) & TAG_MASK;
        if page == 0 { continue; }
        // 最后一页可能没满，只读用到的槽位
        let slots = (slot_count - page_index * SLOTS_PER_PAGE).min(SLOTS_PER_PAGE);
        let bytes = &mut page_buf[..slots * 8];
        if proc.read_into(page, bytes) != bytes.len() { continue; }

        for raw in bytes.chunks_exact(8) {
            let addr = u64::from_le_bytes(raw.try_into().unwrap()) & TAG_MASK;
            if addr == 0 { continue; }   // 墓碑
            let Ok(vptr) = proc.read_ptr(addr) else { continue; };
            let rtti = match names.rtti.get(&vptr) {
                Some(name) => name.clone(),
                None => {
                    let Ok(name) = rtti_name(proc, addr) else { continue; };
                    let name: Arc<str> = name.into();
                    names.rtti.insert(vptr, name.clone());
                    name
                }
            };
            let type_name = read_type_name(proc, addr, &mut names.types).unwrap_or_else(|| "".into());
            let kind = kinds::classify(&type_name);
            let Ok(state_vector) = proc.read_ptr(addr + STATE_VECTOR_PTR_OFFSET) else { continue; };
            if state_vector == 0 { continue; }
            // 碰撞箱：min(xyz) max(xyz) 宽 高。宽高决定框的大小；pos.y - min.y 是 pos 到脚底的高度差
            let shape = aabb.as_ref().and_then(|index| {
                let eid = proc.read_u32(addr + ENTITY_ID_OFFSET).ok()?;
                let elem = index.element(eid, AABB_SHAPE_SIZE)?;
                let mut b = [0u8; AABB_SHAPE_SIZE as usize];
                if proc.read_into(elem, &mut b) != b.len() { return None; }
                let f = |i: usize| f32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
                let pos_y = proc.read_f32(state_vector + 4).ok()?;
                Some(([f(6), f(7)], pos_y - f(1)))
            });
            // 数值必须合理，否则多半是读到了别的东西，退回默认值
            let (size, foot_offset) = shape
                .filter(|([w, h], off)| (0.05..=20.0).contains(w) && (0.05..=20.0).contains(h) && (-0.5..=3.0).contains(off))
                .unwrap_or((DEFAULT_SIZE, 0.0));
            ret.push(Actor { addr, rtti, type_name, kind, size, foot_offset, state_vector });
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
