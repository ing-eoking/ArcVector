use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

pub const NOBODY: u64 = 0;

pub fn mint() -> u64 {
    static OWNER: OnceLock<u64> = OnceLock::new();
    *OWNER.get_or_init(|| {
        let mut h = DefaultHasher::new();
        match node_address() {
            Some(mac) => h.write(&mac),
            None => h.write_u64(unpredictable()),
        }
        h.write_u32(std::process::id());
        h.write_u128(started());
        match h.finish() {
            NOBODY => NOBODY + 1,
            token => token,
        }
    })
}

fn started() -> u128 {
    static AT: OnceLock<u128> = OnceLock::new();
    *AT.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos())
    })
}

fn unpredictable() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

#[cfg(target_os = "linux")]
fn node_address() -> Option<[u8; 6]> {
    let mut names: Vec<_> = std::fs::read_dir("/sys/class/net")
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.file_name()))
        .collect();
    names.sort();
    names.iter().find_map(|name| {
        if name == "lo" {
            return None;
        }
        let path = std::path::Path::new("/sys/class/net")
            .join(name)
            .join("address");
        parse_address(&std::fs::read_to_string(path).ok()?)
    })
}

#[cfg(not(target_os = "linux"))]
fn node_address() -> Option<[u8; 6]> {
    None
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_address(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.trim().split(':');
    for slot in &mut out {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    if parts.next().is_some() || out == [0u8; 6] {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{NOBODY, mint, parse_address};

    #[test]
    fn an_address_reads_back_as_six_bytes() {
        assert_eq!(
            parse_address("02:42:ac:11:00:02\n"),
            Some([0x02, 0x42, 0xac, 0x11, 0x00, 0x02])
        );
    }

    #[test]
    fn what_is_not_an_address_is_refused() {
        assert_eq!(parse_address("00:00:00:00:00:00"), None, "an unset card");
        assert_eq!(parse_address("02:42:ac:11:00"), None, "too few");
        assert_eq!(parse_address("02:42:ac:11:00:02:03"), None, "too many");
        assert_eq!(parse_address("zz:42:ac:11:00:02"), None, "not hex");
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn the_token_is_one_value_for_the_life_of_the_process() {
        let first = mint();
        assert_eq!(first, mint());
        assert_ne!(first, NOBODY, "zero is the token for nobody");
    }
}
