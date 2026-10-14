//! Tabela de processos (Toolhelp32) e resolução de raízes de árvores de processos.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// Nome do executável em minúsculas (ex.: `discord.exe`).
    pub exe: String,
}

#[cfg(windows)]
pub fn snapshot() -> Vec<ProcInfo> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;

    let mut out = Vec::new();
    unsafe {
        let Ok(h) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(h, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push(ProcInfo {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(),
                });
                if Process32NextW(h, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(h);
    }
    out
}

#[cfg(not(windows))]
pub fn snapshot() -> Vec<ProcInfo> {
    vec![]
}

fn find(table: &[ProcInfo], pid: u32) -> Option<&ProcInfo> {
    table.iter().find(|p| p.pid == pid)
}

/// Sobe pelos pais enquanto o pai existir **e** for o mesmo executável.
/// Evita subir até `explorer.exe`/`Update.exe` e lida com subprocessos (Chromium/Electron).
pub fn root_of(table: &[ProcInfo], pid: u32) -> u32 {
    let mut cur = pid;
    for _ in 0..64 {
        let Some(me) = find(table, cur) else { break };
        match find(table, me.ppid) {
            Some(parent) if parent.pid != me.pid && parent.exe == me.exe => cur = parent.pid,
            _ => break,
        }
    }
    cur
}

pub fn exe_of(table: &[ProcInfo], pid: u32) -> Option<&str> {
    find(table, pid).map(|p| p.exe.as_str())
}

#[cfg(test)]
pub fn p(pid: u32, ppid: u32, exe: &str) -> ProcInfo {
    ProcInfo { pid, ppid, exe: exe.to_lowercase() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_stops_at_different_exe() {
        let t = vec![p(1, 0, "explorer.exe"), p(10, 1, "Update.exe"), p(20, 10, "Discord.exe"), p(21, 20, "Discord.exe"), p(22, 21, "Discord.exe")];
        assert_eq!(root_of(&t, 22), 20);
        assert_eq!(root_of(&t, 20), 20);
        assert_eq!(root_of(&t, 10), 10);
        assert_eq!(root_of(&t, 999), 999);
    }

    #[test]
    fn snapshot_contains_self() {
        let t = snapshot();
        assert!(t.iter().any(|p| p.pid == std::process::id()));
    }
}
