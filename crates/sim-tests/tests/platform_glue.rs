//! Platform glue tests that require no production changes.
//!
//! The PowerShell UAC launcher in `discovery_and_os.rs` is Windows-gated and
//! builds its command inline, so it cannot be called from a Linux test. These
//! tests use two layers:
//!   1. a pure local mirror of the command construction, exercised with paths
//!      containing spaces / Unicode / quotes;
//!   2. a compile-time source guard (`include_str!`) that fails if production
//!      drifts back to the nested `-Command` form that broke on spaced paths.
//!
//! Layer 1 tests a *copy* of the construction, not the production function.
//! Layer 2 narrows that gap by checking the actual source text, but it is still
//! a string-level assertion, not a runtime validation of PowerShell.

const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_utf16le_base64(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Mirror of the production firewall script builder (double-quoted netsh rule
/// names and a sanitized program path).
fn build_firewall_script(exe_path: &str) -> String {
    let exe = exe_path.replace('"', "");
    let mut script = String::from(
        "netsh advfirewall firewall delete rule name=\"AudioShare_Ultra_UDP\" | Out-Null\n\
         netsh advfirewall firewall delete rule name=\"AudioShare_Ultra_App\" | Out-Null\n\
         netsh advfirewall firewall add rule name=\"AudioShare_Ultra_UDP\" dir=in action=allow protocol=UDP localport=65530-65532 profile=any | Out-Null\n",
    );
    if !exe.is_empty() {
        script.push_str(&format!(
            "netsh advfirewall firewall add rule name=\"AudioShare_Ultra_App\" dir=in action=allow program=\"{exe}\" profile=any | Out-Null\n"
        ));
    }
    script
}

/// Mirror of the production `Start-Process ... -EncodedCommand` wrapper.
fn build_uac_command(encoded: &str) -> String {
    format!(
        "Start-Process -FilePath 'powershell' -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{encoded}'"
    )
}

#[test]
fn uac_command_survives_spaced_unicode_and_quoted_paths() {
    for exe in [
        r"C:\Program Files\AudioShare Ultra\as-ultra.exe",
        r"C:\Users\First Last\Downloads\Über Audio\as-ultra.exe",
        r"C:\Tools\it's\as-ultra.exe",
        r#"C:\Tools\say"hi"\as-ultra.exe"#,
        "",
    ] {
        let script = build_firewall_script(exe);
        assert!(script.contains("profile=any"));
        assert!(script.contains("AudioShare_Ultra_UDP"));
        if !exe.is_empty() {
            assert!(
                script.contains(&exe.replace('"', "")),
                "exe path lost from generated script: {exe}"
            );
        }

        let encoded = encode_utf16le_base64(&script);
        assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));

        let command = build_uac_command(&encoded);
        assert!(command.contains("-EncodedCommand"), "wrong PowerShell mode");
        assert!(
            !command.contains("'-NoProfile','-NonInteractive','-Command'"),
            "legacy nested -Command form regressed"
        );
        assert!(!command.contains('"'), "raw double quote would break the outer -Command string");
        assert!(command.contains(&encoded), "encoded payload not carried verbatim");
        assert_eq!(
            command.matches('\'').count(),
            10,
            "unexpected quoting structure in UAC command"
        );
    }
}

/// Compile-time drift guard over the actual production source. This does not
/// modify production code; it only fails the test if someone reintroduces the
/// nested-quote launcher that n1-C2 fixed.
#[test]
fn production_source_still_uses_encoded_command() {
    const SOURCE: &str = include_str!("../../host-engine/src/discovery_and_os.rs");
    assert!(
        SOURCE.contains("-EncodedCommand"),
        "production firewall launcher no longer uses -EncodedCommand"
    );
    assert!(
        !SOURCE.contains("'-NoProfile','-NonInteractive','-Command'"),
        "nested -Command launcher regressed in production source"
    );
    assert!(
        !SOURCE.contains("ps_inner.replace"),
        "backslash-escaped nested quoting regressed in production source"
    );
}
