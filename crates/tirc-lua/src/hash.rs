//! The native `tirc.hash` module: fast, deterministic string hashing for Lua
//! consumers (per-nick colors today, anything needing a stable hash later).

use mlua::Lua;

use super::get_or_create_module;

/// Mask keeping hashes exactly representable in a Lua number (f64: 53
/// mantissa bits), so arithmetic on the Lua side is lossless.
const LUA_INT_MASK: u64 = (1 << 53) - 1;

/// XXH3-64 truncated to 53 bits: deterministic, non-negative, and stable
/// across runs and platforms.
fn hash_xxh3(s: &[u8]) -> u64 {
    twox_hash::XxHash3_64::oneshot(s) & LUA_INT_MASK
}

/// Registers the `tirc.hash` module. `hash.xxh3(s)` returns a deterministic
/// non-negative integer for any string.
pub fn create_tirc_hash_lua_module(lua: &Lua) -> anyhow::Result<mlua::Table> {
    let module = get_or_create_module(lua, "tirc.hash")?;

    module.set(
        "xxh3",
        lua.create_function(|_, s: mlua::String| Ok(hash_xxh3(&s.as_bytes())))?,
    )?;

    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_fits_in_a_lua_number() {
        let a = hash_xxh3(b"topaxi");
        assert_eq!(a, hash_xxh3(b"topaxi"));
        assert_ne!(a, hash_xxh3(b"Topaxi"));
        assert!(a <= LUA_INT_MASK);
    }

    #[test]
    fn lua_module_returns_stable_values() {
        let lua = Lua::new();
        create_tirc_hash_lua_module(&lua).unwrap();
        let (a, b, c): (u64, u64, u64) = lua
            .load(
                r#"
                local hash = require('tirc.hash')
                return hash.xxh3('dan'), hash.xxh3('dan'), hash.xxh3('other')
                "#,
            )
            .eval()
            .unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, super::hash_xxh3(b"dan"));
    }
}
