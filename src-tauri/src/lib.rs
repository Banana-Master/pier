mod ssh;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    ssh::cleanup_ext_cache();
    tauri::Builder::default()
        .menu(|h| {
            use tauri::menu::{Menu, PredefinedMenuItem as P, Submenu};
            let app = Submenu::with_items(h, "Pier", true, &[
                &P::about(h, Some("Pier 정보"), None)?,
                &P::separator(h)?,
                &P::services(h, Some("서비스"))?,
                &P::separator(h)?,
                &P::hide(h, Some("Pier 가리기"))?,
                &P::hide_others(h, Some("다른 항목 가리기"))?,
                &P::show_all(h, Some("모두 보기"))?,
                &P::separator(h)?,
                &P::quit(h, Some("Pier 종료"))?,
            ])?;
            let file = Submenu::with_items(h, "파일", true, &[&P::close_window(h, Some("윈도우 닫기"))?])?;
            let edit = Submenu::with_items(h, "편집", true, &[
                &P::undo(h, Some("실행 취소"))?,
                &P::redo(h, Some("다시 실행"))?,
                &P::separator(h)?,
                &P::cut(h, Some("잘라내기"))?,
                &P::copy(h, Some("복사"))?,
                &P::paste(h, Some("붙여넣기"))?,
                &P::select_all(h, Some("전체 선택"))?,
            ])?;
            let view = Submenu::with_items(h, "보기", true, &[&P::fullscreen(h, Some("전체 화면"))?])?;
            let win = Submenu::with_items(h, "윈도우", true, &[
                &P::minimize(h, Some("최소화"))?,
                &P::maximize(h, Some("확대/축소"))?,
            ])?;
            Menu::with_items(h, &[&app, &file, &edit, &view, &win])
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(ssh::AppState::default())
        .invoke_handler(tauri::generate_handler![
            ssh::ssh_connect,
            ssh::ssh_disconnect,
            ssh::ssh_reconnect,
            ssh::ssh_alive,
            ssh::local_open,
            ssh::secret_set,
            ssh::secret_get,
            ssh::secret_delete,
            ssh::term_open,
            ssh::term_write,
            ssh::term_resize,
            ssh::term_close,
            ssh::sftp_home,
            ssh::sftp_realpath,
            ssh::sftp_list,
            ssh::sftp_mkdir,
            ssh::sftp_rename,
            ssh::sftp_remove,
            ssh::sftp_download,
            ssh::sftp_upload,
            ssh::profile_list,
            ssh::profile_save,
            ssh::profile_delete,
            ssh::default_download_dir,
            ssh::sys_stats,
            ssh::sftp_find,
            ssh::sftp_stat,
            ssh::sys_procs,
            ssh::proc_kill,
            ssh::sftp_copy,
            ssh::tmux_cwd,
            ssh::remote_chmod,
            ssh::remote_archive,
            ssh::remote_extract,
            ssh::key_generate,
            ssh::key_install,
            ssh::ssh_test,
            ssh::tunnel_start,
            ssh::tunnel_stop,
            ssh::log_candidates,
            ssh::log_start,
            ssh::log_stop,
            ssh::docker_list,
            ssh::docker_action,
            ssh::ext_apps,
            ssh::ext_open,
            ssh::ext_resolve,
            ssh::notify_os,
            ssh::ssh_config_hosts,
            ssh::sftp_read_text,
            ssh::sftp_write_text,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
