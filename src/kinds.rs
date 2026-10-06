//! 实体类型 → 分类 → 框的颜色
//!
//! 类型名来自客户端 Actor +0x228 的 HashedString（例如 "minecraft:cow"），见 game::read_type_name

/// 实体分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 敌对生物
    Hostile,
    /// 友好 / 中立生物
    Passive,
    /// 其他玩家
    Player,
    /// 掉落物
    Item,
    /// 不画框：经验球、箭、雪球、烟花等投射物和效果实体
    Ignored,
}

/// 敌对生物（去掉 "minecraft:" 前缀后的名字）
const HOSTILE: &[&str] = &[
    "zombie", "zombie_villager", "zombie_villager_v2", "husk", "drowned", "skeleton", "stray", "bogged",
    "wither_skeleton", "creeper", "spider", "cave_spider", "enderman", "endermite", "silverfish", "witch",
    "slime", "magma_cube", "phantom", "blaze", "ghast", "guardian", "elder_guardian", "shulker", "vex",
    "pillager", "vindicator", "evoker", "ravager", "piglin_brute", "hoglin", "zoglin", "warden", "breeze",
    "creaking", "zombie_pigman", "wither", "ender_dragon",
];

/// 不需要框的实体
const IGNORED: &[&str] = &[
    "xp_orb", "arrow", "snowball", "egg", "ender_pearl", "eye_of_ender_signal", "fireball", "small_fireball",
    "dragon_fireball", "wither_skull", "wither_skull_dangerous", "fireworks_rocket", "thrown_trident",
    "splash_potion", "lingering_potion", "xp_bottle", "fishing_hook", "area_effect_cloud", "lightning_bolt",
    "leash_knot", "painting", "falling_block", "tnt", "shulker_bullet", "llama_spit", "wind_charge_projectile",
    "breeze_wind_charge_projectile", "evocation_fang",
];

pub fn classify(type_name: &str) -> Kind {
    let name = type_name.strip_prefix("minecraft:").unwrap_or(type_name);
    // 玩家的类型名带着皮肤信息，例如 "minecraft:player.0.persona-8d9660b86dffe437-9"
    if name == "player" || name.starts_with("player.") { return Kind::Player; }
    match name {
        "item" => Kind::Item,
        _ if HOSTILE.contains(&name) => Kind::Hostile,
        _ if IGNORED.contains(&name) => Kind::Ignored,
        // 读不到类型名的实体也不画，免得框住一些不知道是什么的东西
        "" => Kind::Ignored,
        _ => Kind::Passive,
    }
}

/// 框的颜色（非预乘 RGBA）
pub fn color(kind: Kind) -> [f32; 4] {
    match kind {
        Kind::Hostile => [1.0, 0.2, 0.2, 1.0],   // 红
        Kind::Passive => [0.3, 1.0, 0.3, 1.0],   // 绿
        Kind::Player => [0.3, 0.6, 1.0, 1.0],    // 蓝
        Kind::Item => [1.0, 0.85, 0.2, 1.0],     // 黄
        Kind::Ignored => [0.0; 4],
    }
}

/// 框内的填充色：只给敌对生物填一层半透明的红色，让它们更显眼
pub fn fill(kind: Kind) -> Option<[f32; 4]> {
    match kind {
        Kind::Hostile => Some([1.0, 0.2, 0.2, 0.15]),
        _ => None,
    }
}
