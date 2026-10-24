//! 极简 Base64（RFC 4648 标准字母表，含 `=` 填充）。
//!
//! 客户端刻意不引入第三方 crate，因此这里自带一份编解码实现：
//! 协议要求所有可能含换行/冒号/中文的字段都做 Base64 编码，避免分帧被破坏。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 Base64 编码。
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 标准 Base64 解码；容忍空白字符与缺失填充。
pub fn decode(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;

    for ch in input.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => return Err(format!("非法的 Base64 字符: {:?}", other as char)),
        } as u32;

        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_ascii() {
        for case in ["", "a", "ab", "abc", "hello world", "中文测试"] {
            let encoded = encode(case.as_bytes());
            assert_eq!(decode(&encoded).unwrap(), case.as_bytes());
        }
    }

    #[test]
    fn known_vectors() {
        assert_eq!(encode(b"hello"), "aGVsbG8=");
        assert_eq!(encode(b"hel"), "aGVs");
        assert_eq!(decode("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn decode_tolerates_whitespace() {
        assert_eq!(decode("aGVs\nbG8=").unwrap(), b"hello");
    }
}
