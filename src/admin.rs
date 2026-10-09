//! Admin-privilege detection and self-elevation (packet capture via ETW
//! requires an elevated process).

use anyhow::{Result, bail};

/// Is the current process running elevated?
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut return_length = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut return_length,
            );
            ok.is_ok() && elevation.TokenIsElevated != 0
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// Relaunch the current program with a UAC elevation prompt.
/// The caller should exit after this returns successfully.
pub fn relaunch_elevated() -> Result<()> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let exe = std::env::current_exe()?;
        let args = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

        let verb: Vec<u16> = "runas\0".encode_utf16().collect();
        let file: Vec<u16> = exe
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let params: Vec<u16> = if args.is_empty() {
            vec![0]
        } else {
            args.encode_utf16().chain(std::iter::once(0)).collect()
        };

        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR(params.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };

        if result.0 as isize <= 32 {
            bail!("elevation was declined or failed (code {:?})", result.0 as isize);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        bail!("automatic elevation is only supported on Windows; please rerun with appropriate privileges")
    }
}
