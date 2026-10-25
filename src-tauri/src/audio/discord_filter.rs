//! Política de exclusão do Discord: calcula, a partir da tabela de processos, qual estratégia
//! de captura WASAPI garante que o áudio do Discord nunca entre na transmissão.

use super::process_tree::{exe_of, root_of, ProcInfo};

pub fn default_excluded() -> Vec<String> {
    vec!["discord.exe".into(), "discordptb.exe".into(), "discordcanary.exe".into()]
}

pub fn normalize(names: &[String]) -> Vec<String> {
    names.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturePlan {
    /// `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` apontando para `root`.
    Exclude { root: u32 },
    /// Uma captura `INCLUDE_TARGET_PROCESS_TREE` por raiz (mixadas). Lista ordenada.
    Include { roots: Vec<u32> },
    /// Fonte proibida (ex.: a janela escolhida é o próprio Discord): sem áudio.
    Blocked,
}

impl CapturePlan {
    pub fn describe(&self) -> String {
        match self {
            CapturePlan::Exclude { root } => format!("EXCLUDE_TREE(pid {root})"),
            CapturePlan::Include { roots } => format!("INCLUDE_TREE x{} {:?}", roots.len(), roots),
            CapturePlan::Blocked => "BLOCKED".into(),
        }
    }
}

pub fn is_excluded(table: &[ProcInfo], pid: u32, names: &[String]) -> bool {
    exe_of(table, pid).is_some_and(|e| names.iter().any(|n| n == e))
}

/// Raízes das árvores de processos cujo executável está na lista (pai não pertence à lista).
pub fn family_roots(table: &[ProcInfo], names: &[String]) -> Vec<u32> {
    let in_list = |p: &ProcInfo| names.iter().any(|n| *n == p.exe);
    let mut roots: Vec<u32> = table
        .iter()
        .filter(|p| in_list(p))
        .filter(|p| !table.iter().any(|q| q.pid == p.ppid && q.pid != p.pid && in_list(q)))
        .map(|p| p.pid)
        .collect();
    roots.sort_unstable();
    roots
}

/// Modo Tela Inteira: todo o áudio do sistema, exceto o do Discord (e exceto o próprio Luke-Share
/// quando `self_is_playing` está ativo reproduzindo a live de um amigo).
pub fn plan_system(
    table: &[ProcInfo],
    names: &[String],
    self_pid: u32,
    self_is_playing: bool,
    audio_session_pids: impl FnOnce() -> Vec<u32>,
) -> CapturePlan {
    let roots = family_roots(table, names);
    let self_root = root_of(table, self_pid);
    match roots.len() {
        // Sem Discord: exclui a árvore do próprio app → áudio de todo o sistema sem eco.
        0 => CapturePlan::Exclude { root: self_root },
        1 if !self_is_playing => CapturePlan::Exclude { root: roots[0] },
        _ => {
            let mut out: Vec<u32> = audio_session_pids()
                .into_iter()
                .filter(|pid| *pid != self_pid && !is_excluded(table, *pid, names))
                .map(|pid| root_of(table, pid))
                .filter(|r| *r != self_root && !is_excluded(table, *r, names))
                .collect();
            out.sort_unstable();
            out.dedup();
            CapturePlan::Include { roots: out }
        }
    }
}

/// Modo Aplicativo Específico: captura exclusivamente a árvore do processo da janela.
pub fn plan_window(table: &[ProcInfo], names: &[String], window_pid: u32) -> CapturePlan {
    if is_excluded(table, window_pid, names) {
        return CapturePlan::Blocked;
    }
    let root = root_of(table, window_pid);
    if is_excluded(table, root, names) {
        return CapturePlan::Blocked;
    }
    CapturePlan::Include { roots: vec![root] }
}

#[cfg(test)]
mod tests {
    use super::super::process_tree::p;
    use super::*;

    fn names() -> Vec<String> {
        normalize(&default_excluded())
    }

    fn table() -> Vec<ProcInfo> {
        vec![
            p(4, 0, "System"),
            p(100, 4, "explorer.exe"),
            p(200, 100, "Update.exe"),
            p(210, 200, "Discord.exe"),
            p(211, 210, "Discord.exe"),
            p(212, 210, "Discord.exe"),
            p(300, 100, "chrome.exe"),
            p(301, 300, "chrome.exe"),
            p(400, 100, "DiscordCanary.exe"),
            p(401, 400, "DiscordCanary.exe"),
        ]
    }

    #[test]
    fn roots_found_with_update_exe_parent() {
        assert_eq!(family_roots(&table(), &names()), vec![210, 400]);
        assert_eq!(family_roots(&table()[..7], &names()), vec![210]);
    }

    #[test]
    fn system_plan_no_discord_excludes_self() {
        let t: Vec<_> = table().into_iter().filter(|p| !p.exe.starts_with("discord") && p.exe != "update.exe").collect();
        assert_eq!(plan_system(&t, &names(), 999, false, Vec::new), CapturePlan::Exclude { root: 999 });
    }

    #[test]
    fn system_plan_single_discord() {
        let t = &table()[..7];
        assert_eq!(plan_system(t, &names(), 999, false, || panic!("não deve consultar sessões")), CapturePlan::Exclude { root: 210 });
        // Quando o próprio Luke-Share está reproduzindo áudio WASAPI ao mesmo tempo que o Discord está aberto,
        // exclui ambos (Discord + self_pid 999) via Include mixer!
        let t_with_chrome_child = &table()[..8];
        assert_eq!(plan_system(t_with_chrome_child, &names(), 999, true, || vec![301, 211, 999]), CapturePlan::Include { roots: vec![300] });
    }

    #[test]
    fn system_plan_multi_root_uses_include_mixer() {
        // sessões ativas: chrome (filho), discord (filho) e canary → só chrome sobra, pela raiz
        let plan = plan_system(&table(), &names(), 999, false, || vec![301, 211, 401, 300]);
        assert_eq!(plan, CapturePlan::Include { roots: vec![300] });
    }

    #[test]
    fn window_plan() {
        let t = table();
        assert_eq!(plan_window(&t, &names(), 301), CapturePlan::Include { roots: vec![300] });
        assert_eq!(plan_window(&t, &names(), 212), CapturePlan::Blocked);
        assert_eq!(plan_window(&t, &names(), 401), CapturePlan::Blocked);
    }

    #[test]
    fn custom_names_are_case_insensitive() {
        assert_eq!(normalize(&[" Discord_Test.EXE ".into(), "".into()]), vec!["discord_test.exe"]);
    }
}
