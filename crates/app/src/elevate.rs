//! Running as administrator (Windows): whether DupeMap does, and starting it again so that it
//! does (Windows asks the user first).

/// Does this process run with administrator rights? (Always `true` where there is no such
/// thing to ask for.)
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use std::ffi::c_void;
    #[link(name = "advapi32")]
    extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(token: *mut c_void, class: u32, info: *mut c_void, len: u32, returned: *mut u32) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn CloseHandle(h: *mut c_void) -> i32;
    }
    // SAFETY: Win32 calls with a correctly sized out-parameter; the token handle is closed.
    unsafe {
        let mut token: *mut c_void = std::ptr::null_mut();
        // TOKEN_QUERY
        if OpenProcessToken(GetCurrentProcess(), 0x0008, &mut token) == 0 {
            return false;
        }
        // TokenElevation = 20 → TOKEN_ELEVATION { TokenIsElevated: u32 }
        let mut elevated: u32 = 0;
        let mut got = 0u32;
        let ok = GetTokenInformation(token, 20, &mut elevated as *mut u32 as *mut c_void, 4, &mut got) != 0;
        CloseHandle(token);
        ok && elevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    // Linux: root (effective user id 0); the scan only reads what this user can read otherwise
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:")).map(|l| l.split_whitespace().nth(2) == Some("0")))
        .unwrap_or(true)
}

/// Quote one command-line argument the way Windows programs split them.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
                continue;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
                continue;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Start this program again as administrator with `args`. `Ok` once the new one is starting
/// (this one should then close); an error when the user said no or it could not be started.
#[cfg(windows)]
pub fn restart_elevated(args: &[String]) -> Result<(), String> {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut c_void;
    }
    let wide = |s: &OsStr| -> Vec<u16> { s.encode_wide().chain(std::iter::once(0)).collect() };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let params: Vec<String> = args.iter().map(|a| quote_arg(a)).collect();
    let (verb, file, params) = (wide(OsStr::new("runas")), wide(exe.as_os_str()), wide(OsStr::new(&params.join(" "))));
    // SAFETY: all strings are NUL-terminated UTF-16 that live across the call.
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), 1)
    };
    // values above 32 mean it started; 5 (access denied) is what declining the prompt gives
    match r as usize {
        n if n > 32 => Ok(()),
        5 => Err("Windows did not give administrator rights".into()),
        n => Err(format!("could not start DupeMap as administrator (error {n})")),
    }
}

#[cfg(not(windows))]
pub fn restart_elevated(_args: &[String]) -> Result<(), String> {
    Err("Run DupeMap with sudo to scan as root".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_windows() {
        assert_eq!(quote_arg("--scan"), "--scan");
        assert_eq!(quote_arg(r"C:\Users\A B\dupemap.db"), r#""C:\Users\A B\dupemap.db""#);
        assert_eq!(quote_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(""), r#""""#);
    }
}
