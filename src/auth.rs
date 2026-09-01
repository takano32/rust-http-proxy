#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub credentials: Option<String>, // "username:password"
}

impl AuthConfig {
    pub fn new(credentials: Option<String>) -> Self {
        Self { credentials }
    }

    pub fn is_enabled(&self) -> bool {
        self.credentials.is_some()
    }

    pub fn validate(&self, proxy_auth_header: Option<&str>) -> bool {
        let expected = match self.credentials {
            Some(ref c) => c,
            None => return true,
        };

        let header = match proxy_auth_header {
            Some(h) => h.trim(),
            None => return false,
        };

        if !header.starts_with("Basic ") && !header.starts_with("basic ") {
            return false;
        }

        let encoded = header[6..].trim();
        let decoded = match base64_decode(encoded) {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => return false,
            },
            None => return false,
        };

        decoded == *expected
    }
}

pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[i8] = &[
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, // 0-15
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, // 16-31
        -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 62, -1, -1, -1, 63, // 32-47
        52, 53, 54, 55, 56, 57, 58, 59, 60, 61, -1, -1, -1, -1, -1, -1, // 48-63
        -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, // 64-79
        15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, -1, -1, -1, -1, -1, // 80-95
        -1, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, // 96-111
        41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, -1, -1, -1, -1, -1, // 112-127
    ];

    let bytes = input.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);

    let mut buf = 0u32;
    let mut bits = 0;

    for &b in bytes {
        if b >= 128 {
            return None;
        }
        let val = TABLE[b as usize];
        if val == -1 {
            return None;
        }
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_decode() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert_eq!(
            String::from_utf8(base64_decode("dXNlcjpwYXNz").unwrap()).unwrap(),
            "user:pass"
        );
    }

    #[test]
    fn test_auth_validation() {
        let auth = AuthConfig::new(Some("admin:secret123".to_string()));
        assert!(auth.is_enabled());

        // Valid auth
        assert!(auth.validate(Some("Basic YWRtaW46c2VjcmV0MTIz")));
        // Invalid pass
        assert!(!auth.validate(Some("Basic YWRtaW46d3Jvbmc=")));
        // Missing header
        assert!(!auth.validate(None));
        // Wrong scheme
        assert!(!auth.validate(Some("Bearer token123")));
    }
}
